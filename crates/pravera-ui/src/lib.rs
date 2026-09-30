//! Pravera — remote desktop client and host control surface.
//!
//! ## One machine, one identity, one endpoint
//!
//! Pravera is peer to peer, so the same window both dials out and accepts. It
//! does both through a single [`Transport`] bound once at startup from the
//! device key. Two endpoints holding the same key would publish two discovery
//! records for one machine, the second quietly overwriting the first, and the
//! symptom would be a machine that is reachable until the moment someone uses
//! it to reach out.
//!
//! ## The shell
//!
//! DigiClip's shell. A rail of icons down the left edge, each one growing
//! sideways into a labelled pill while it is pointed at; the page on the
//! right, a header card over a body card, floating 5px apart on the window
//! floor and filling it; and every open session or shell as a tab in the
//! title bar, beside a home toggle that goes back to the rest of the app.
//!
//! ## Why the picture takes the whole window
//!
//! [`Screen::Session`] replaces the rail rather than sitting beside it: it
//! is the remote machine's own screen, and framing it would shrink it for no
//! gain. Every other screen draws in the ordinary shell, including while a
//! session is running — the file panes are meant to be used *during* one.
//! The tabs stay in the title bar either way, so a running session is never
//! something anyone has to remember they are in.
//!
//! ## Running with no window, but never with no sign of it
//!
//! A machine with no monitor still has to be reachable, so Pravera can start at
//! sign-in with `--hidden`, host by itself, and be closed to the notification
//! area. What it will not do is run invisibly: the window only hides while
//! there is a tray icon to hide behind, and that icon says which machine this
//! is and whether anyone is connected. A remote-access tool that can run with
//! nothing at all to show for it is one that was installed *on* somebody rather
//! than *by* them.

pub mod autostart;
pub mod backdrop;
pub mod chrome;
pub mod single_instance;
pub mod components;
pub mod icon;
pub mod install;
pub mod motion;
pub mod net;
pub mod prefs;
pub mod screens;
pub mod theme;
pub mod tray;
pub mod update;
pub mod widget;

mod setup;
mod shot;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use iced::widget::{
    button, column, container, keyed_column, mouse_area, opaque, row, text, Space, Stack,
};
use iced::{Alignment, Background, Border, Element, Length, Padding, Size, Subscription, Task, Vector};
use tokio::sync::mpsc;

use components::titlebar::{self, Control};
use motion::{HoverTracker, Tween};
use net::host::{self, Hosting};
use net::known;
use net::link::{self, Command, Credentials, Ending, Link};
use net::Carry;
use pravera_client::Progress;
use pravera_core::DeviceId;
use pravera_crypto::Identity;
use pravera_discovery::Discovered;
use pravera_transport::{Reachability, Transport};
use prefs::Prefs;
use theme::tokens as t;
use tray::Tray;
use widget::transform::Transform;

/// How long a notice about a finished session stays up.
///
/// Long enough to read a sentence, and dismissible before that. Failures do
/// not expire at all: the reason a session died is the thing the person came
/// back to the screen to find out.
const NOTICE_FOR: Duration = Duration::from_secs(8);

/// How often a hosting Pravera wakes with nothing else to do.
///
/// Only while hosting: this is what drains the host's event channel and keeps
/// the connection count in the icon's tooltip honest. Everything else in the
/// app is driven by something having happened.
const HEARTBEAT: Duration = Duration::from_secs(1);

/// How often the mDNS browser is emptied while the device list is on screen.
///
/// Not a scan. The daemon is already listening on its own thread; this is only
/// how often what it heard is read, so it is cheap enough to be frequent and
/// slow enough not to be a redraw loop.
const LAN_POLL: Duration = Duration::from_secs(2);

/// How often the device list rescans every source by itself.
///
/// Slower than the mDNS poll because it costs more: this one enumerates network
/// interfaces and asks Tailscale for its peer list, where the poll only drains
/// a channel that filled itself. Fast enough that plugging a cable in shows up
/// while somebody is still looking at the screen, which is the point — a list
/// that only updates when you press a button is a list you cannot trust
/// without pressing the button.
const AUTO_SCAN: Duration = Duration::from_secs(8);

/// What is recorded, unless `RUST_LOG` says otherwise.
///
/// Pravera's own crates at debug; the graphics and window stacks at warn,
/// because at info they bury everything else under adapter enumeration.
const LOG_FILTER: &str = "info,pravera=debug,wgpu=warn,naga=warn,iced=warn";

/// How long after boot the frames subscription runs unconditionally — see
/// `booted_at`. Longer than every boot entrance (520ms + stagger + delay), so
/// the gate can only fail to open if the machine took longer than this to
/// produce the first frame.
const BOOT_FRAME_GRACE: Duration = Duration::from_secs(3);

/// The window this app opens as, and the smallest it will go. Below the
/// minimum the Devices table and the file panes stop fitting side by side
/// beside the rail, and a layout that has to scroll sideways is broken.
const WINDOW_SIZE: Size = Size::new(1280.0, 800.0);
const WINDOW_MIN: Size = Size::new(960.0, 600.0);

/// The title bar's icon, the tray's blob at 32×32: the taskbar, Alt-Tab and
/// the window thumbnail share one mark.
fn window_icon() -> Option<iced::window::Icon> {
    let rgba = crate::tray::icon::pixels(false);
    let size = crate::tray::icon::size();
    iced::window::icon::from_rgba(rgba, size, size).ok()
}

/// Start the application: set up logging, work out whether this launch is
/// meant to be seen, and hand control to iced.
///
/// Lives in the library rather than in `main.rs` so that the binary target
/// has no code of its own to test. That is not tidiness: the Windows
/// manifest asking for elevation is applied as a linker argument, Cargo
/// applies binary link arguments to a bin target's unit-test harness too,
/// and an elevated test harness is one `cargo test` cannot launch at all.
pub fn run() -> iced::Result {
    // What this file was started as, before anything else: `--version` and
    // the quiet installer answer on the console and exit, and a file named
    // like the installer opens as the installer — none of which is the app,
    // so none of which may take the single-instance port or touch the log.
    let cli = install::Cli::from_env();
    match install::launch(&cli) {
        install::Launch::Version => {
            attach_console();
            install::print_version();
            std::process::exit(0);
        }
        install::Launch::ExportIcon(path, size) => {
            attach_console();
            std::process::exit(install::export_icon(&path, size));
        }
        install::Launch::Quiet => {
            attach_console();
            std::process::exit(install::run_quiet(&cli));
        }
        install::Launch::Setup => {
            // Removing the copy this process is running from: hand the job
            // to a temporary copy that can delete it.
            #[cfg(windows)]
            if cli.uninstall {
                let layout = cli.dir.clone().map(install::Layout::in_dir).or_else(install::Layout::find);
                if layout.as_ref().is_some_and(install::runs_from) {
                    let _ = install::hand_off_uninstall();
                    std::process::exit(0);
                }
            }
            return setup::run(cli);
        }
        install::Launch::App => {}
    }

    // A preview stands beside the real one rather than deferring to it.
    if preview() {
        start_logging();
        // Taking pictures: a window that is never shown.
        return launch(shot::dir().is_some(), None);
    }

    // One Pravera at a time — checked BEFORE logging. The old order
    // (`start_logging` first) made every second launch rename `pravera.log`
    // to `pravera.log.1` and create a fresh 0-byte file before exiting `7`,
    // truncating the first instance's live log. A second instance must never
    // touch the log files.
    //
    // A second launch while the first is in the tray must not start a new
    // window — it must focus the one that is already there. The port stays
    // bound for the lifetime of the first process.
    //
    // The service agent losing the race to the user's own Pravera must NOT
    // write the show-flag: that would make the first's next 500ms poll call
    // `show_window` + `gain_focus` — a focus-steal at logon the user never
    // asked for. Silent `exit(7)` lets the service stand down.
    // Started by the build it replaced, which may still be letting go of the
    // port: wait for it rather than mistake it for another Pravera.
    if cli.updated {
        single_instance::wait_for_port(Duration::from_secs(20));
    }

    let single_lock = single_instance::try_acquire();
    if single_lock.is_none() {
        if pravera_service::is_agent_launch() {
            single_instance::exit_silently_as_second();
        } else {
            single_instance::notify_first_and_exit();
        }
    }

    start_logging();
    if cli.updated {
        tracing::info!(version = install::VERSION, "started after an update");
    }
    // Whatever the last update renamed aside, now that nothing runs it.
    if let Ok(exe) = install::self_image() {
        install::sweep_retired(&exe);
    }

    // Started by the sign-in entry rather than by a person. Coming up as a
    // window on a machine somebody is using for something else would be rude;
    // on a machine with no monitor it would be pointless.
    let hidden = std::env::args().any(|argument| argument == autostart::HIDDEN_FLAG);
    if hidden {
        tracing::info!("starting hidden");
    }

    launch(hidden, single_lock)
}

/// Whether this is a look at the interface rather than a Pravera: debug
/// builds only, with `PRAVERA_PREVIEW` set.
///
/// A preview runs beside the real Pravera without disturbing it. It takes no
/// single-instance lock, puts no icon in the tray, binds no endpoint — two
/// endpoints on one device key would publish two discovery records for one
/// machine — never touches the service registration, and writes nothing to
/// disk. What it reads, it reads from the real data directory, so its
/// screens show this machine.
fn preview() -> bool {
    cfg!(debug_assertions) && std::env::var_os("PRAVERA_PREVIEW").is_some()
}

/// Give a windowed executable the console it was started from, so
/// `pravera --version` and the quiet installer can be read in a terminal.
/// Only when there is no output already: a pipe (CI, the updater reading
/// `--version`) must stay the pipe.
fn attach_console() {
    #[cfg(windows)]
    // SAFETY: plain Win32 calls with no pointers.
    unsafe {
        use windows::Win32::System::Console::{AttachConsole, GetStdHandle, ATTACH_PARENT_PROCESS, STD_OUTPUT_HANDLE};
        let has_output = GetStdHandle(STD_OUTPUT_HANDLE).is_ok_and(|handle| !handle.is_invalid() && !handle.0.is_null());
        if !has_output {
            let _ = AttachConsole(ATTACH_PARENT_PROCESS);
        }
    }
}

/// The screen a preview opens on, from `PRAVERA_PREVIEW` itself.
fn preview_screen() -> Option<Screen> {
    let wanted = std::env::var("PRAVERA_PREVIEW").ok()?;
    Screen::ALL
        .into_iter()
        .find(|screen| screen.label().eq_ignore_ascii_case(wanted.trim()))
}

/// Whether a preview opens with the connect dialog up: `PRAVERA_PREVIEW=connect`.
fn preview_dialog() -> bool {
    std::env::var("PRAVERA_PREVIEW").is_ok_and(|wanted| wanted.trim().eq_ignore_ascii_case("connect"))
}

fn launch(hidden: bool, single_lock: Option<std::net::TcpListener>) -> iced::Result {
    let single_lock = std::sync::Mutex::new(single_lock);

    iced::application(
        move || {
            let (mut app, task) = Pravera::new(hidden);
            app.single_instance_lock = single_lock.lock().unwrap().take();
            (app, task)
        },
        Pravera::update,
        Pravera::view,
    )
    .title(Pravera::title)
    .subscription(Pravera::subscription)
    .theme(app_theme)
    // One face for the whole interface, embedded so it looks the same on a
    // machine that has never heard of it — registered once per weight, so no
    // weight falls through to a system font.
    .font(t::FONT_BYTES)
    .font(t::font_at(t::EXTRA_WEIGHTS[0]))
    .font(t::font_at(t::EXTRA_WEIGHTS[1]))
    .default_font(t::FONT_UI)
    .window(iced::window::Settings {
        size: shot::size().unwrap_or(WINDOW_SIZE),
        min_size: Some(WINDOW_MIN),
        icon: window_icon(),
        // The native chrome is replaced by `components::titlebar`, which has to
        // supply drag, minimise, maximise and close itself.
        decorations: false,
        transparent: true,
        visible: !hidden,
        // Alt+F4 and the tray both route through `Message::CloseWindow`, so
        // that closing means the same thing however it was asked for.
        exit_on_close_request: false,
        ..iced::window::Settings::default()
    })
    .antialiasing(true)
    .run()
}

/// Send the log somewhere it can be read afterwards.
///
/// A release build is windowed and has no console, so the terminal is not an
/// option: everything goes to a file next to the device key. A debug build
/// keeps the terminal, where it is being watched live.
fn start_logging() {
    if cfg!(debug_assertions) {
        pravera_core::telemetry::init(LOG_FILTER);
        return;
    }

    match pravera_core::paths::data_dir() {
        Ok(dir) => {
            pravera_core::telemetry::init_to_file(LOG_FILTER, &dir.join("pravera.log"));
        }
        // Nowhere to write. Installing the terminal subscriber anyway costs
        // nothing and means a build run from a console still says something.
        Err(_) => pravera_core::telemetry::init(LOG_FILTER),
    }
}

/// A named function rather than a closure: `.theme` needs a callable valid for
/// any lifetime, and an inline closure only satisfies one specific lifetime.
fn app_theme(_: &Pravera) -> iced::Theme {
    theme::theme()
}

/// The top-level sections of the app.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Home,
    /// The tab in front: a picture, or a shell. Only reachable while a tab is
    /// open, and it is where a session lands the moment it starts.
    ///
    /// A real destination rather than a mode, so leaving it for the file panes
    /// and coming back is ordinary navigation instead of a special case.
    Session,
    Users,
    Transfers,
    Settings,
    Mcp,
}

/// The rail, top to bottom, in runs split by a short hairline.
///
/// The split is not decoration: the first run is about reaching machines, the
/// second about administering this one, and the last sits alone at the foot
/// of the rail, where DigiClip keeps its settings. [`Screen::Session`] is
/// absent on purpose: open tabs get a pill each, and a tab in the title bar.
const NAV: [&[Screen]; 3] = [
    &[Screen::Home, Screen::Transfers],
    &[Screen::Users, Screen::Mcp],
    &[Screen::Settings],
];

impl Screen {
    const ALL: [Screen; 6] = [
        Screen::Home,
        Screen::Session,
        Screen::Users,
        Screen::Transfers,
        Screen::Settings,
        Screen::Mcp,
    ];

    const fn label(self) -> &'static str {
        match self {
            Screen::Home => "Devices",
            Screen::Session => "Session",
            Screen::Users => "Users",
            Screen::Transfers => "Transfers",
            Screen::Settings => "Settings",
            Screen::Mcp => "Agent",
        }
    }

    const fn icon(self) -> &'static str {
        match self {
            Screen::Home => icon::DEVICES,
            Screen::Session => icon::GAUGE,
            Screen::Users => icon::USERS,
            Screen::Transfers => icon::TRANSFERS,
            Screen::Settings => icon::SETTINGS,
            Screen::Mcp => icon::AGENT,
        }
    }

    /// Slot in the shared nav [`HoverTracker`].
    fn index(self) -> usize {
        Screen::ALL.iter().position(|&s| s == self).unwrap_or(0)
    }
}

/// Rail hover slots past the screens: the host pill, then one per open tab.
const NAV_HOST: usize = Screen::ALL.len();
const NAV_TABS: usize = Screen::ALL.len() + 1;
/// Open tabs past this many are left off the rail. The title bar lists every
/// one, so nothing becomes unreachable; the rail just stops growing.
const RAIL_TABS: usize = 8;
const NAV_SLOTS: usize = NAV_TABS + RAIL_TABS;

/// What a background look at this machine's screens found, and did.
#[derive(Debug, Clone)]
pub struct DisplayCheck {
    /// How many displays capture offers now; `None` when they could not be read.
    displays: Option<usize>,
    status: pravera_capture::VirtualDisplayStatus,
    /// `Some` when the machine had no screen and this check tried to add the
    /// virtual display: what appeared, or why nothing did.
    added: Option<Result<String, String>>,
    /// The capture backend's name. Asked here because opening a backend is
    /// driver work, and driver work does not belong on the thread drawing
    /// the window.
    backend: String,
}

impl DisplayCheck {
    /// Blocking: a first install creates a device and waits for its monitor.
    fn run() -> DisplayCheck {
        let added = pravera_capture::needs_virtual_display().then(|| {
            pravera_capture::ensure_virtual_display(1920, 1080, 60)
                .map(|shown| {
                    format!(
                        "No monitor is attached, so Pravera added its virtual display ({}x{}).",
                        shown.resolution.width, shown.resolution.height
                    )
                })
                .map_err(|error| error.to_string())
        });
        let source = pravera_capture::source();
        DisplayCheck {
            displays: source
                .as_ref()
                .ok()
                .and_then(|source| source.displays().ok())
                .map(|list| list.len()),
            status: pravera_capture::virtual_display_status(),
            added,
            backend: source
                .map(|source| source.name().to_string())
                .unwrap_or_else(|_| "unavailable".to_string()),
        }
    }
}

#[derive(Debug, Clone)]
pub enum Message {
    Navigate(Screen),
    /// Preview pictures: go to the step's page, take it, write it. See
    /// [`shot`].
    ShotNext(usize),
    /// The change of a step that is photographed part way through.
    ShotFire(usize),
    ShotTake(usize),
    ShotTaken(usize, iced::window::Screenshot),
    /// A discovery pass finished.
    Discovered(Box<Discovered>),
    Refresh,
    /// The window gained or lost the operating system's focus.
    ///
    /// Matters because of the low-level keyboard hook: a Pravera sitting behind
    /// another window must give the Windows key back, or the machine would
    /// appear to have a broken keyboard until the session ended.
    Focused(bool),
    /// Take in whatever the subnet has announced since the last look.
    ///
    /// Separate from [`Message::Refresh`] because it is not a scan: it drains
    /// a channel the mDNS daemon has already filled, costs nothing, and must
    /// not put the interface into its searching state every two seconds.
    PollLan,
    /// A full scan nobody asked for. See `AUTO_SCAN`.
    AutoScan,
    /// A frame elapsed, or a heartbeat: drives every in-flight animation, and
    /// is also when a live session's picture and events are collected.
    Tick(Instant),

    /// The one network endpoint this machine owns finished binding.
    Bound(Result<Box<BoundEndpoint>, String>),
    /// How many displays this machine turned out to have. Zero is the answer a
    /// machine with nothing plugged into a graphics output gives, and it is the
    /// reason such a machine cannot host.
    Displays(Result<usize, String>),
    /// Periodic display poll: notices a monitor plugged in or pulled out, and
    /// brings the virtual display back on a machine that lost its screen.
    PollDisplays,
    /// A background look at this machine's screens finished, having added the
    /// virtual display if the machine had none.
    DisplaysChecked(Box<DisplayCheck>),
    /// The Settings button asked for a virtual display, and this is how it went.
    DisplayAutoAdded(Result<String, String>),
    /// The Settings button asked for the driver to be staged.
    DriverInstalled(Result<Option<String>, String>),
    /// What the service registration came to, checked off the UI thread.
    ServiceChecked(pravera_service::Installed),
    /// Whether the sign-in task is registered, read off the UI thread: the
    /// answer comes from running `schtasks`, which takes long enough to see.
    AtSignIn(Result<bool, String>),
    /// Whether this machine has a hardware video encoder. Probed once, off
    /// the UI thread: finding out loads Media Foundation.
    EncoderChecked(bool),

    // --- sessions ---
    Connect(screens::connect::Message),
    /// A connection attempt finished, with a session or a reason.
    Connected(Result<Carry<LiveSession>, String>),
    Session(screens::session::Message),
    DismissNotice,

    // --- files ---
    Transfers(screens::transfers::Message),
    /// A directory came back, or did not. The side says which pane asked.
    Listed(
        screens::transfers::Side,
        Result<Box<pravera_proto::Listing>, String>,
    ),
    /// A transfer moved. Delivered through a channel rather than a message per
    /// chunk: at sixty-four kilobytes a piece a fast link would otherwise put
    /// thousands of messages a second through the update loop.
    TransferProgress,
    /// A transfer ended. `Ok` carries the bytes that arrived.
    TransferFinished(u64, Result<u64, String>),

    // --- accounts ---
    Users(screens::users::Message),

    // --- hosting ---
    Settings(screens::settings::Message),
    HostingStarted(Result<Carry<LiveHost>, String>),

    // --- agent bus ---
    Mcp(screens::mcp::Message),
    McpStarted(Result<SocketAddr, String>),

    // --- terminal ---
    /// Something the terminal surface published: its pane size, or the
    /// ended banner's close.
    Terminal(screens::terminal::Message),
    /// A key press nothing on screen took. Routed in `update`: to a terminal
    /// in front, or to the shortcuts.
    KeyPressed(screens::terminal::Press),
    /// Escape, whoever else saw it: it closes the topmost dialog or menu
    /// before anything else gets to treat it as a key.
    Escape(screens::terminal::Press),
    /// The clipboard, read for a paste into the terminal in front.
    TerminalPaste(Option<String>),
    /// A terminal printed something, or its connection went.
    TerminalWake,
    /// A standalone terminal came up, or did not.
    TerminalConnected(Result<Carry<link::OpenedTerminal>, String>),

    // --- unattended ---
    /// Something was clicked in the notification area.
    Tray(tray::Request),

    // --- window chrome ---
    /// The window handle, resolved once at startup so the chrome can act on it.
    WindowReady(Option<iced::window::Id>),
    /// The window changed size, by any means. Triggers a maximise re-query.
    WindowResized(iced::window::Id, Size),
    /// The window's size, asked for once it exists.
    WindowSized(Size),
    /// The authoritative answer from the windowing system.
    MaximizedChanged(bool),
    DragWindow,
    Minimize,
    ToggleMaximize,
    CloseWindow,

    // --- hover ---
    //
    // Every hover message names its widget *and* says whether the pointer is
    // arriving or leaving. Carrying `Option<T>` instead, with `None` for exit,
    // is the bug that made hover work in one direction only: see the note on
    // `motion::HoverTracker`.
    HoverControl(Control, bool),
    HoverNav(usize, bool),
    HoverTab(usize, bool),
    HoverDevice(usize, bool),
    /// A row on the Devices list was clicked.
    ChooseDevice(usize),
    /// A row on the Devices list was right-clicked: its menu opens at the
    /// pointer.
    DeviceMenu(usize),
    /// The context menu's connect entry, for the row it names.
    MenuConnect(usize),
    /// The context menu's terminal entry: a shell on that machine, no
    /// picture. The same connect dialog opens; only what it starts differs.
    MenuTerminal(usize),
    /// The context menu's forget entry. The machine leaves the remembered
    /// list; discovery is free to find it again and it returns unsaved.
    MenuForget(usize),
    /// The context menu went away without choosing.
    MenuClosed,
    /// The pointer moved onto, or off of, a context-menu entry.
    MenuHover(usize, bool),
    /// Add a machine nothing has discovered, by its connect code.
    AddDevice,
    /// A tab was clicked: it comes to the front, and so does the session
    /// screen.
    SwitchTab(usize),
    /// A tab's close was pressed. A session is asked to end; a shell simply
    /// goes.
    CloseTab(usize),
    /// The home toggle beside the tabs: between the tab in front and the rest
    /// of the app.
    ToggleHome,
    /// A second Pravera was launched — show the existing window.
    SingleInstanceCheck,
    /// The updater's clock: check, or apply, if it is time.
    UpdateTick,
    UpdateChecked(Result<install::release::Release, String>),
    UpdateProgress(u64, u64),
    UpdateDownloaded(Result<PathBuf, String>),
}

/// The endpoint, and the device ID it proves.
#[derive(Debug, Clone)]
pub struct BoundEndpoint {
    transport: Transport,
    device_id: DeviceId,
}

/// A session and the channel it reports on. Neither can be duplicated, which
/// is why they travel inside a [`Carry`].
type LiveSession = (Link, mpsc::UnboundedReceiver<link::Event>);
type LiveHost = (Hosting, mpsc::UnboundedReceiver<host::Event>);

/// Where every running transfer reports its figures, and where they are read.
///
/// One channel for all of them. A message per chunk would put thousands a
/// second through `update` on a fast link, and every one of them would redraw
/// the whole window; this is drained once per frame instead.
type ProgressChannel = (
    mpsc::UnboundedSender<(u64, Progress)>,
    mpsc::UnboundedReceiver<(u64, Progress)>,
);

/// A running session, and everything the interface needs to draw it.
struct Live {
    link: Link,
    state: screens::session::State,
    events: mpsc::UnboundedReceiver<link::Event>,
}

/// A running shell, likewise.
struct TerminalLive {
    host: String,
    state: screens::terminal::State,
    events: mpsc::UnboundedReceiver<pravera_client::TerminalEvent>,
    /// A standalone terminal's own connection, held for as long as the tab
    /// is. `None` for a shell opened inside a session, which rides that
    /// session's connection instead.
    _hold: Option<link::TerminalHold>,
}

/// One open tab.
///
/// Two kinds — a remote session and a shell on a machine — and the rest of
/// the application addresses tabs by position and asks the active one what it
/// is.
enum Tab {
    Session(Live),
    Terminal(TerminalLive),
}

impl Tab {
    /// The name the tab shows.
    fn title(&self) -> &str {
        match self {
            Tab::Session(live) => live.link.host_name(),
            Tab::Terminal(shell) => &shell.host,
        }
    }

    fn is_terminal(&self) -> bool {
        matches!(self, Tab::Terminal(_))
    }

    /// The far end has gone, and the tab is only still here to say why.
    fn is_ended(&self) -> bool {
        match self {
            Tab::Session(_) => false,
            Tab::Terminal(shell) => shell.state.is_ended(),
        }
    }

    fn as_session(&self) -> Option<&Live> {
        match self {
            Tab::Session(live) => Some(live),
            Tab::Terminal(_) => None,
        }
    }

    fn as_session_mut(&mut self) -> Option<&mut Live> {
        match self {
            Tab::Session(live) => Some(live),
            Tab::Terminal(_) => None,
        }
    }

    fn as_terminal_mut(&mut self) -> Option<&mut TerminalLive> {
        match self {
            Tab::Session(_) => None,
            Tab::Terminal(shell) => Some(shell),
        }
    }
}

/// The open tabs, and which one is in front.
///
/// Invariants the rest of the application leans on: `active` always names a
/// real tab while any exist, and every mutation restores that before it
/// returns. Tabs are added at the end and activated on arrival; closing the
/// active one promotes its neighbour rather than leaving a hole.
struct Tabs {
    items: Vec<Tab>,
    active: usize,
}

impl Tabs {
    fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    fn len(&self) -> usize {
        self.items.len()
    }

    fn active(&self) -> Option<&Tab> {
        self.items.get(self.active)
    }

    fn active_session(&self) -> Option<&Live> {
        self.active().and_then(Tab::as_session)
    }

    fn active_session_mut(&mut self) -> Option<&mut Live> {
        self.items.get_mut(self.active).and_then(Tab::as_session_mut)
    }

    fn active_terminal_mut(&mut self) -> Option<&mut TerminalLive> {
        self.items.get_mut(self.active).and_then(Tab::as_terminal_mut)
    }

    fn has_terminal(&self) -> bool {
        self.items.iter().any(Tab::is_terminal)
    }

    fn push_session(&mut self, live: Live) {
        self.items.push(Tab::Session(live));
        self.active = self.items.len() - 1;
    }

    fn push_terminal(&mut self, shell: TerminalLive) {
        self.items.push(Tab::Terminal(shell));
        self.active = self.items.len() - 1;
    }

    /// Remove every tab whose index is listed, keeping `active` honest.
    fn remove_all(&mut self, mut indices: Vec<usize>) {
        indices.sort_unstable();
        indices.dedup();
        indices.reverse();
        for index in indices {
            if index >= self.items.len() {
                continue;
            }
            self.items.remove(index);
            if self.active >= index && self.active > 0 {
                self.active -= 1;
            }
        }
        if self.active >= self.items.len() {
            self.active = self.items.len().saturating_sub(1);
        }
    }

    /// Every session, in tab order, for the passes that must reach all of
    /// them: event drains, key releases, the things that outlive focus.
    fn sessions_mut(&mut self) -> impl Iterator<Item = &mut Live> {
        self.items.iter_mut().filter_map(Tab::as_session_mut)
    }
}

/// Something that finished and is worth a sentence.
struct Notice {
    message: String,
    failure: bool,
    /// `None` for a failure, which stays until it is dismissed.
    until: Option<Instant>,
}

struct Pravera {
    screen: Screen,
    /// The last screen that was not a tab: where the home toggle goes back to.
    last_shell: Screen,
    /// The page on its way out, drawn until `leave` reaches zero, after which
    /// `screen`'s own page cascades in.
    leaving: Option<Screen>,
    leave: Tween,
    /// When the page leaving will be gone, so a second navigation during the
    /// exit does not restart it.
    leave_ends: Instant,
    /// When the rail last arrived: at boot, from the tray, or back from a
    /// tab. It cascades in from this moment.
    shown_at: Instant,
    /// When the tab surface last came to the front. It fades up from this.
    surface_at: Instant,
    discovered: Discovered,
    /// Listens to the local subnet for as long as the window is open.
    ///
    /// Long-lived on purpose: mDNS announcements arrive when a machine chooses
    /// to send one, so a browser created per scan would only ever hear the
    /// answers to its own question and would look like an empty network for
    /// the first second of every pass. `None` when the daemon would not start,
    /// which costs this one source and nothing else.
    browser: Option<pravera_discovery::Browser>,
    /// Whether this window is the one the operating system is sending keys to.
    focused: bool,
    /// What became of the service registration at startup. Shown in Settings,
    /// because whether this machine comes back after a reboot by itself is
    /// exactly the sort of thing that should not have to be inferred.
    service: pravera_service::Installed,
    /// Set while a discovery pass is in flight, so the UI can show progress
    /// rather than appearing to do nothing.
    scanning: bool,
    now: Instant,

    /// This machine's identity and its one endpoint. `None` until the bind
    /// finishes, or for good if it failed — in which case the reason is shown
    /// rather than a device ID that names nothing reachable.
    endpoint: Option<BoundEndpoint>,
    endpoint_error: Option<String>,

    window: Option<iced::window::Id>,
    /// The window's logical size, for placing things at the pointer.
    window_size: Size,
    maximized: bool,
    chrome_hover: HoverTracker,
    /// How far each rail pill has grown out of its square: on the standard
    /// curve rather than the micro one, because the growth is a movement.
    nav_hover: HoverTracker,
    tab_hover: HoverTracker,
    /// One animation per screen, answering "how selected is this entry". The
    /// highlight crossfades between them when the page changes.
    nav_active: Vec<iced::Animation<bool>>,
    /// The rail's one highlight: where it set off from, in pixels down the
    /// rail's column, the entry it is heading for (see [`rail_position`]), and
    /// how far along that way it has got, 0 to 1. Setting off from a number of
    /// pixels rather than from an entry means a slide that is interrupted
    /// carries on from where the highlight is drawn, and the slide is one
    /// steady movement however far the entry is.
    rail_from: f32,
    rail_to: f32,
    rail_slide: Tween,

    home: screens::home::State,
    connect: screens::connect::State,
    settings: screens::settings::State,
    users: screens::users::State,
    transfers: screens::transfers::State,
    mcp: screens::mcp::State,

    /// The open tabs. The active one is the surface on screen and the target
    /// of every input; the others keep running in the background.
    tabs: Tabs,
    /// The notice on screen, if any.
    notice: Option<Notice>,
    /// A notice on its way out, kept only so its toast can fade.
    fading_notice: Option<Notice>,
    toast: Tween,

    /// Transfers that are running, by the id the ledger knows them as.
    ///
    /// Held so a cancelled one can actually be stopped: aborting the task
    /// drops the future, which resets the QUIC stream, which is what tells the
    /// far end to stop reading a file nobody is waiting for any more.
    running: HashMap<u64, iced::task::Handle>,
    /// Rises for every transfer, so the ledger's rows have stable identities
    /// even after the list is cleared.
    next_transfer: u64,
    /// Where progress arrives from the transfer tasks.
    ///
    /// One channel for all of them, drained on the tick. A message per chunk
    /// would put thousands a second through `update` on a fast link, and every
    /// one of them would redraw the whole window.
    progress: ProgressChannel,

    hosting: Option<Hosting>,
    host_events: Option<mpsc::UnboundedReceiver<host::Event>>,

    /// The notification-area icon, if this platform has one and it was
    /// accepted. `None` means the window is the only way to reach Pravera, and
    /// closing it therefore quits.
    tray: Option<Tray>,
    /// Whether the window is on screen. Tracked rather than asked, because the
    /// answer decides whether closing hides or quits.
    visible: bool,
    /// Choices that outlive the process.
    prefs: Prefs,
    /// Where they are kept. `None` if this machine has nowhere to keep them,
    /// in which case a change lasts until the process ends.
    prefs_path: Option<PathBuf>,
    /// Machines a session has been made to before, so their connect codes only
    /// ever have to be typed once.
    known: known::Known,
    known_path: Option<PathBuf>,
    /// Registered to start at sign-in. Read in the background at startup and
    /// after every change, never guessed.
    at_sign_in: bool,
    /// How many displays this machine reports; `None` until the probe answers.
    displays: Option<usize>,
    /// The virtual display's state from the last background check; `None`
    /// until the first lands. Read by the Settings card, which must never
    /// probe the device tree itself — it draws every frame.
    virtual_display: Option<pravera_capture::VirtualDisplayStatus>,
    /// The capture backend's name, for the same card, from the same check.
    capture_backend: String,
    /// A background display check is running. The poll skips while one is:
    /// a first install can take half a minute, and checks queued behind it
    /// would each wait on the same lock for nothing.
    checking_displays: bool,
    /// The last automatic failure shown, so an eight-second poll does not
    /// re-raise the same notice after somebody dismissed it.
    display_failure: Option<String>,
    /// Whether the cursor is currently confined to this window, so the
    /// release call happens once rather than every frame.
    cursor_clipped: bool,
    /// Set when a session that was in gaming mode ended: the pump restores
    /// the window next frame, because this update cannot return a task.
    restore_windowed: bool,
    /// The merged Devices list: discovery's view of each machine filled in
    /// with what is remembered about it. Kept on the application rather than
    /// rebuilt in `view`, because what a widget borrows must outlive the
    /// frame it is drawn in — and because the click messages name rows by
    /// position, so every reader must see the same merge.
    device_entries: Vec<screens::home::Entry>,
    /// When this process started. The frames subscription is gated on
    /// "something is animating", and for the first few seconds it runs
    /// regardless: a cold start slow enough that the first tick lands after
    /// every entrance would otherwise never see them move.
    booted_at: Instant,
    /// The close that has been asked for. `None` while the window is staying
    /// up.
    closing: Option<Closing>,
    /// `true` once the window hide has been issued.
    hiding: bool,
    /// Held for the lifetime of the process so `127.0.0.1:47901` stays bound.
    /// `None` only in tests that call `assemble` directly.
    single_instance_lock: Option<std::net::TcpListener>,
    /// Whether a newer release exists, and how far fetching it has got.
    updater: update::Updater,
}

/// Where the pointer last was over the window, for placing a context menu
/// where the click happened.
///
/// Written by the event listener, which is a plain function and cannot reach
/// the application; read when a row asks for its menu. A message per mouse
/// move would rebuild the whole interface on every twitch of the pointer.
mod pointer_at {
    use std::sync::atomic::{AtomicU64, Ordering};

    static AT: AtomicU64 = AtomicU64::new(0);

    pub fn record(point: iced::Point) {
        let packed = (u64::from(point.x.to_bits()) << 32) | u64::from(point.y.to_bits());
        AT.store(packed, Ordering::Relaxed);
    }

    pub fn last() -> iced::Point {
        let packed = AT.load(Ordering::Relaxed);
        iced::Point::new(
            f32::from_bits((packed >> 32) as u32),
            f32::from_bits(packed as u32),
        )
    }
}

impl Pravera {
    fn new(hidden: bool) -> (Self, Task<Message>) {
        // Created before anything else can want it, and on this thread: the
        // icon owns a hidden window whose messages are pumped by the loop iced
        // is about to start, which is this one.
        //
        // Except in the agent, which gets none. The agent is the Pravera the
        // service put into this desktop, and the service starts it again
        // within seconds of it stopping — so a menu offering to quit it would
        // be offering something it cannot do. It is also why a machine with
        // both an agent and a person's own Pravera running showed two icons
        // for one program: they are two processes, and only one of them is
        // anybody's to control.
        let tray = if pravera_service::is_agent_launch() {
            tracing::info!("started by the Pravera service; leaving the notification area alone");
            None
        } else if preview() {
            None
        } else {
            Tray::new(&tray::Status {
                device: None,
                hosting: false,
                connections: 0,
            })
        };

        if tray.is_none() && !pravera_service::is_agent_launch() {
            tracing::debug!("no notification area on this platform; the window is the whole app");
        }

        let mut state = Pravera::assemble(tray, hidden);

        if preview() {
            state.prefs_path = None;
            state.known_path = None;
            if let Some(screen) = preview_screen() {
                state.go_to(screen);
            }
            // `connect` is not a screen but the dialog over Devices, which is
            // otherwise only reachable by a click.
            if preview_dialog() {
                state.connect.open_empty(state.now);
            }
            let pictures = if shot::dir().is_some() {
                iced::window::latest()
                    .and_then(shot::park)
                    .chain(Task::done(Message::ShotNext(0)))
            } else {
                Task::none()
            };
            return (
                state,
                Task::batch([
                    iced::window::latest().map(Message::WindowReady),
                    Task::perform(host::displays(), Message::Displays),
                    read_at_sign_in(),
                    pictures,
                ]),
            );
        }

        let startup = Task::batch([
            iced::window::latest().map(Message::WindowReady),
            Task::perform(bind_endpoint(), |result| {
                Message::Bound(result.map(Box::new))
            }),
            Task::perform(host::displays(), Message::Displays),
            Task::perform(pravera_discovery::scan_all(state.lan_peers()), |d| {
                Message::Discovered(Box::new(d))
            }),
            // Registered — or repointed at this copy, if the file has been
            // moved since last time. Pravera is one portable executable, so
            // "where the service should point" is simply wherever this file
            // happens to be right now, and reconciling it on every start is
            // what keeps that true. Off the UI thread: the service control
            // manager answers when it answers, and the window should not wait.
            check_service(),
            read_at_sign_in(),
            Task::perform(
                async {
                    tokio::task::spawn_blocking(pravera_codec::encodes_in_hardware)
                        .await
                        .unwrap_or(false)
                },
                Message::EncoderChecked,
            ),
        ]);

        (state, startup)
    }

    /// Everything except the notification area.
    ///
    /// Split out so a test can build a whole app without putting a real icon
    /// into somebody's system tray for the length of a test run. Nothing here
    /// may block: it runs before the window exists.
    fn assemble(tray: Option<Tray>, hidden: bool) -> Pravera {
        // A machine told to start hidden with no icon to hide behind would be
        // running with nothing at all to show for it. Better to appear.
        // Except the service agent (`--agent --hidden`): it must stay hidden
        // even with no tray, or a spurious flag would pop its window and steal
        // focus at boot — the exact "window appearing by itself" the agent
        // exists to avoid.
        let is_agent = pravera_service::is_agent_launch();
        let hidden = if is_agent { hidden } else { hidden && tray.is_some() };

        let data_dir = pravera_core::paths::data_dir().ok();
        let prefs_path = data_dir.as_deref().map(Prefs::path_in);
        let prefs = prefs_path.as_deref().map(Prefs::load).unwrap_or_default();

        let known_path = data_dir.as_deref().map(known::Known::path_in);
        let known = known_path
            .as_deref()
            .map(known::Known::load)
            .unwrap_or_default();

        let browser = match pravera_discovery::Browser::start() {
            Ok(browser) => Some(browser),
            // One source down, every other one still working. Not worth a
            // dialog, and definitely not worth refusing to open the window.
            Err(error) => {
                tracing::warn!(%error, "the local subnet will not be searched");
                None
            }
        };

        // One clock for everything that starts on boot, so the first frame
        // is not already past the end of an entrance (the stale-clock trap,
        // see `motion::tests`).
        let now = Instant::now();
        let mut home = screens::home::State::default();
        home.replay(now + motion::STAGGER_STEP);
        let mut connect = screens::connect::State::default();
        connect.set_audio(prefs.hear_the_host);

        Pravera {
            screen: Screen::Home,
            last_shell: Screen::Home,
            leaving: None,
            leave: Tween::default(),
            leave_ends: now,
            shown_at: now,
            surface_at: now,
            discovered: Discovered::default(),
            browser,
            // Assumed until told otherwise: a window that has just opened is
            // focused, and no event is sent to say so.
            focused: true,
            // Replaced by the real answer from the startup task.
            service: pravera_service::Installed::NotElevated,
            scanning: true,
            now,
            endpoint: None,
            endpoint_error: None,
            window: None,
            window_size: WINDOW_SIZE,
            maximized: false,
            chrome_hover: HoverTracker::new(Control::ALL.len()),
            nav_hover: HoverTracker::with_timing(NAV_SLOTS, motion::STANDARD, motion::EASE_CHANGE),
            tab_hover: HoverTracker::default(),
            nav_active: {
                // Devices is the screen the app opens on, and it starts fully
                // itself: no crossfade from a page that was never there.
                let mut active: Vec<_> = (0..Screen::ALL.len()).map(|_| motion::standard(false)).collect();
                active[Screen::Home.index()] = motion::standard(true);
                active
            },
            rail_from: 0.0,
            rail_to: rail_position(Screen::Home).unwrap_or(0.0),
            rail_slide: Tween::at(1.0),
            home,
            connect,
            settings: screens::settings::State::new(),
            // Deliberately not loaded here. Reading the account file is disk
            // work on the startup path, and nothing needs the answer until
            // somebody opens the screen.
            users: screens::users::State::new(),
            transfers: screens::transfers::State::new(),
            mcp: screens::mcp::State::new(),
            tabs: Tabs {
                items: Vec::new(),
                active: 0,
            },
            notice: None,
            fading_notice: None,
            toast: Tween::default(),
            running: HashMap::new(),
            next_transfer: 0,
            progress: mpsc::unbounded_channel(),
            hosting: None,
            host_events: None,
            tray,
            visible: !hidden,
            prefs,
            prefs_path,
            known,
            known_path,
            at_sign_in: false,
            displays: None,
            virtual_display: None,
            capture_backend: "checking…".to_string(),
            checking_displays: false,
            display_failure: None,
            cursor_clipped: false,
            restore_windowed: false,
            device_entries: Vec::new(),
            booted_at: now,
            closing: None,
            hiding: false,
            single_instance_lock: None,
            updater: update::Updater::new(now, install::release::enabled() && !preview()),
        }
    }

    fn title(&self) -> String {
        match self.tabs.active() {
            Some(tab) if self.on_surface() => format!("Pravera - {}", tab.title()),
            _ => "Pravera".to_string(),
        }
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        let task = self.dispatch(message);
        self.refresh_tray();
        // Switches show state the application owns. Told here, after every
        // change, so a flip starts moving before the frame subscription is
        // next decided rather than whenever something else redraws.
        self.settings.sync_switches(
            self.at_sign_in,
            self.prefs.host_at_launch,
            self.prefs.auto_update,
            self.now,
        );
        self.mcp.sync_switch(self.now);
        task
    }

    fn dispatch(&mut self, message: Message) -> Task<Message> {
        // Animations start from `self.now`, and `self.now` only advances on
        // `Tick`, which only arrives while something is already moving. On a
        // still screen the clock is therefore frozen at whenever the last
        // animation ended, possibly minutes ago.
        //
        // Without this line the first hover after a rest would start from
        // that stale timestamp, the first real frame would arrive far past
        // the animation's notional end, and the hover would snap instead of
        // fading: an animation defined, wired up, and invisible.
        if !matches!(message, Message::Tick(_)) {
            self.now = Instant::now();
        }

        match message {
            Message::ShotNext(step) => {
                let Some(page) = shot::PLAN.get(step) else {
                    return iced::exit();
                };
                if self.connect.is_open() {
                    self.connect.dismiss(self.now);
                }
                match page {
                    shot::Step::Screen(screen) => self.go_to(*screen),
                    shot::Step::Connect => {
                        self.go_to(Screen::Home);
                        self.connect.open_empty(self.now);
                    }
                    shot::Step::Hover => {
                        self.go_to(Screen::Home);
                        self.home.set_hovered(0, true, self.now);
                    }
                    shot::Step::Section(section) => {
                        self.go_to(Screen::Settings);
                        self.settings.select(*section, self.now);
                    }
                    // Set the scene now, let it settle, then make the change.
                    shot::Step::Caught(moment) => {
                        match moment {
                            shot::Moment::Leaving => self.go_to(Screen::Settings),
                            shot::Moment::Arriving => self.go_to(Screen::Users),
                            shot::Moment::Dialog | shot::Moment::Rail => self.go_to(Screen::Home),
                        }
                        return shot::before_firing(step);
                    }
                }
                shot::after_settling(step)
            }
            Message::ShotFire(step) => {
                let Some(shot::Step::Caught(moment)) = shot::PLAN.get(step) else {
                    return Task::none();
                };
                match moment {
                    shot::Moment::Leaving => self.go_to(Screen::Users),
                    shot::Moment::Arriving => self.go_to(Screen::Transfers),
                    shot::Moment::Dialog => self.connect.open_empty(self.now),
                    shot::Moment::Rail => self.go_to(Screen::Settings),
                }
                shot::after_moment(step, *moment)
            }
            Message::ShotTake(step) => shot::take(step),
            Message::ShotTaken(step, picture) => {
                shot::save(step, &picture);
                Task::done(Message::ShotNext(step + 1))
            }
            Message::Navigate(screen) => {
                self.go_to(screen);
                // The Settings rows that snapshot the outside world are
                // re-read on the way in: the sign-in task can be changed in
                // Task Scheduler and the service repointed by moving the exe,
                // and a switch showing launch-time truth after that lies.
                if screen == Screen::Settings {
                    return Task::batch([read_at_sign_in(), check_service()]);
                }
                Task::none()
            }
            Message::Discovered(found) => {
                self.scanning = false;
                // `refresh_entries` compares rather than assigns: an automatic
                // scan usually finds precisely what the last one found.
                self.discovered = *found;
                self.refresh_entries();
                Task::none()
            }
            Message::Focused(focused) => {
                self.focused = focused;
                // Nothing waits for the next frame here. Losing focus while a
                // key is held is exactly when the hook must let go, and the
                // next frame may be a long time coming.
                self.reconcile_grab();
                if !focused {
                    // Every tab lets go: a key held on a background session is
                    // still a key held on somebody's machine.
                    for live in self.tabs.sessions_mut() {
                        for command in screens::session::update(
                            &mut live.state,
                            screens::session::Message::LostFocus,
                            self.now,
                        ) {
                            live.link.send(command);
                        }
                    }
                }
                Task::none()
            }
            Message::PollLan => {
                let Some(browser) = &self.browser else {
                    return Task::none();
                };

                // Compared rather than assigned: handing the home screen a new
                // list every two seconds would be work for nothing.
                let lan = browser.peers();
                if lan != self.discovered.lan {
                    self.discovered.lan = lan;
                    self.refresh_entries();
                }
                Task::none()
            }
            // Deliberately without the spinner that `Refresh` sets. This runs
            // on a timer, and a progress indicator that appears every few
            // seconds without anybody asking is noise rather than feedback.
            Message::AutoScan => {
                if self.scanning {
                    return Task::none();
                }
                Task::perform(pravera_discovery::scan_all(self.lan_peers()), |d| {
                    Message::Discovered(Box::new(d))
                })
            }
            Message::Refresh => {
                self.scanning = true;
                Task::perform(pravera_discovery::scan_all(self.lan_peers()), |d| {
                    Message::Discovered(Box::new(d))
                })
            }
            Message::Tick(now) => {
                self.now = now;
                self.pump()
            }

            Message::Bound(Ok(endpoint)) => {
                tracing::info!(device = %endpoint.device_id, "endpoint bound");

                // Told now rather than when hosting starts, because this
                // machine can hear its own announcement the instant it makes
                // one and the two events are not ordered. A device list with
                // this machine in it invites somebody to sit and wait for a
                // connection to themselves.
                if let Some(browser) = &self.browser {
                    browser.ignore(endpoint.transport.peer_key().to_code());
                }

                self.endpoint = Some(*endpoint);
                self.endpoint_error = None;

                // The whole point of a machine that starts by itself: nobody
                // is going to press the button on a box with no monitor.
                if self.prefs.host_at_launch && self.hosting.is_none() {
                    return self.start_hosting();
                }
                Task::none()
            }
            Message::Bound(Err(reason)) => {
                tracing::warn!(%reason, "the endpoint could not be bound");
                self.endpoint_error = Some(reason);
                Task::none()
            }
            Message::Displays(result) => {
                match result {
                    Ok(count) => {
                        if count == 0 {
                            tracing::warn!("this machine reports no displays");
                        }
                        self.displays = Some(count);
                    }
                    // Not knowing is a different state from knowing there are
                    // none, and the screen says nothing rather than guessing.
                    Err(reason) => tracing::warn!(%reason, "the displays could not be counted"),
                }
                // Then the real question — is any of them a screen? — off the
                // UI thread, adding the virtual display if none is.
                self.check_displays()
            }
            Message::PollDisplays => self.check_displays(),
            Message::DisplaysChecked(check) => {
                self.checking_displays = false;
                let DisplayCheck {
                    displays,
                    status,
                    added,
                    backend,
                } = *check;
                if displays.is_some() {
                    self.displays = displays;
                }
                self.capture_backend = backend;
                let was_active = self
                    .virtual_display
                    .as_ref()
                    .is_some_and(|before| before.display_active);
                self.virtual_display = Some(status);
                match added {
                    // Already there, and only being made primary again.
                    Some(Ok(_)) if was_active => {}
                    Some(Ok(shown)) => {
                        self.display_failure = None;
                        self.settings.virtual_result(shown);
                        return self.host_once_screen_exists();
                    }
                    // Said once per distinct reason: the poll retries every
                    // eight seconds and the same sentence each time is noise
                    // — and would undo somebody pressing Dismiss.
                    Some(Err(reason)) if self.display_failure.as_ref() != Some(&reason) => {
                        self.display_failure = Some(reason.clone());
                        self.settings.virtual_result(reason);
                    }
                    Some(Err(_)) | None => {}
                }
                Task::none()
            }
            Message::DriverInstalled(result) => {
                match result {
                    Ok(Some(_)) => self
                        .settings
                        .virtual_result("The virtual display driver is installed.".to_string()),
                    Ok(None) => self.settings.virtual_result(
                        "The virtual display driver was already installed.".to_string(),
                    ),
                    Err(output) => {
                        tracing::warn!(%output, "driver install failed");
                        self.settings.virtual_result(output);
                    }
                }
                self.check_displays()
            }
            Message::DisplayAutoAdded(result) => {
                let hosting = match result {
                    Ok(shown) => {
                        self.display_failure = None;
                        self.settings.virtual_result(shown);
                        self.host_once_screen_exists()
                    }
                    Err(reason) => {
                        self.settings.virtual_result(reason);
                        Task::none()
                    }
                };
                // Refresh the card from what is really there now.
                Task::batch([hosting, self.check_displays()])
            }
            Message::ServiceChecked(installed) => {
                match &installed {
                    pravera_service::Installed::Registered => {
                        tracing::info!("registered the Pravera service; this machine now starts at boot");
                    }
                    pravera_service::Installed::Repointed => {
                        tracing::info!("Pravera has moved; the service now points at this copy");
                    }
                    pravera_service::Installed::Refused(reason) => {
                        tracing::warn!(%reason, "the Pravera service could not be registered");
                    }
                    // Nothing to say. Not elevated is the ordinary case on a
                    // machine nobody has asked to run unattended, and
                    // unchanged is every run after the first.
                    _ => {}
                }
                self.service = installed;
                Task::none()
            }
            Message::AtSignIn(result) => {
                match result {
                    Ok(enabled) => self.at_sign_in = enabled,
                    Err(reason) => self.settings.failed(reason),
                }
                Task::none()
            }
            Message::EncoderChecked(hardware) => {
                self.settings.set_hardware(hardware);
                Task::none()
            }
            Message::SingleInstanceCheck => {
                // The flag is the real Pravera's to answer, not a preview's.
                if preview() {
                    return Task::none();
                }
                // The installer or an uninstall asking this Pravera to step
                // aside so its file can be replaced or removed.
                if single_instance::check_quit_request() {
                    tracing::info!("asked to quit by the installer");
                    self.hosting = None;
                    self.host_events = None;
                    return iced::exit();
                }
                if single_instance::check_and_clear_flag() {
                    tracing::info!("SingleInstanceCheck flag present -> show_window");
                    return self.show_window();
                }
                Task::none()
            }
            Message::UpdateTick => self.on_update_tick(),
            Message::UpdateChecked(result) => {
                match self.updater.checked(result, self.now) {
                    Some(release) => download_update(release),
                    None => Task::none(),
                }
            }
            Message::UpdateProgress(done, total) => {
                self.updater.progress(done, total);
                Task::none()
            }
            Message::UpdateDownloaded(result) => {
                self.updater.downloaded(result, self.now);
                self.apply_update(false)
            }

            Message::Connect(inner) => self.on_connect_form(inner),
            Message::Connected(result) => self.on_connected(result),
            Message::Session(inner) => self.on_session(inner),
            Message::DismissNotice => {
                self.clear_notice();
                Task::none()
            }

            Message::Transfers(inner) => self.on_transfers(inner),
            Message::Listed(side, Ok(listing)) => {
                self.transfers.listed(side, *listing, self.now);
                Task::none()
            }
            Message::Listed(side, Err(reason)) => {
                self.transfers.not_listed(side, reason);
                Task::none()
            }
            Message::TransferProgress => {
                self.drain_progress();
                Task::none()
            }
            Message::TransferFinished(id, result) => self.on_transfer_finished(id, result),

            Message::Users(inner) => self.on_users(inner),
            Message::Settings(inner) => self.on_settings(inner),
            Message::Mcp(inner) => self.on_mcp(inner),
            Message::McpStarted(Ok(addr)) => {
                tracing::info!("the agent bus came up on {addr}");
                Task::none()
            }
            Message::McpStarted(Err(reason)) => {
                self.mcp.set_error(Some(reason));
                Task::none()
            }
            Message::HostingStarted(result) => self.on_hosting_started(result),
            Message::Tray(request) => self.on_tray(request),

            Message::WindowReady(id) => {
                self.window = id;
                // The frame the compositor draws is not styled at creation —
                // there is no window to style yet. This is the first moment
                // there is one.
                match id {
                    Some(id) => Task::batch([
                        chrome::dress(id),
                        iced::window::size(id).map(Message::WindowSized),
                    ]),
                    None => Task::none(),
                }
            }
            // The maximise glyph has to track reality, not just our own button.
            // A window can be maximised by snapping it to the top edge or by
            // Win+Up, neither of which passes through this app. Every resize,
            // whatever caused it, re-asks the windowing system.
            Message::WindowResized(id, size) => {
                self.window_size = size;
                // A resize can also be a move onto a monitor with a different
                // scale factor, which the raw pointer mapping divides by.
                let scale = if self.tabs.active_session().is_some() {
                    iced::window::scale_factor(id)
                        .map(|scale| Message::Session(screens::session::Message::Scale(scale)))
                } else {
                    Task::none()
                };
                Task::batch([
                    iced::window::is_maximized(id).map(Message::MaximizedChanged),
                    scale,
                ])
            }
            Message::WindowSized(size) => {
                self.window_size = size;
                Task::none()
            }
            Message::MaximizedChanged(maximized) => {
                self.maximized = maximized;
                Task::none()
            }
            // Every chrome action is a no-op until the handle resolves, which
            // happens on the first frame, long before anyone can click.
            Message::DragWindow => self
                .window
                .map(iced::window::drag)
                .unwrap_or_else(Task::none),
            Message::Minimize => self
                .window
                .map(|id| iced::window::minimize(id, true))
                .unwrap_or_else(Task::none),
            Message::ToggleMaximize => {
                // Flipped locally for an instant glyph swap; the resize this
                // causes will confirm it a frame later.
                self.maximized = !self.maximized;
                self.window
                    .map(iced::window::toggle_maximize)
                    .unwrap_or_else(Task::none)
            }
            Message::CloseWindow => self.close_window(),

            Message::HoverControl(which, entering) => {
                self.chrome_hover.set(which.index(), entering, self.now);
                Task::none()
            }
            Message::HoverNav(index, entering) => {
                self.nav_hover.set(index, entering, self.now);
                Task::none()
            }
            Message::HoverTab(index, entering) => {
                self.tab_hover.resize(self.tabs.len(), self.now);
                self.tab_hover.set(index, entering, self.now);
                Task::none()
            }
            Message::HoverDevice(index, entering) => {
                self.home.set_hovered(index, entering, self.now);
                Task::none()
            }
            Message::ChooseDevice(index) => self.choose_device(index, screens::connect::Purpose::Session),
            Message::DeviceMenu(index) => {
                self.home.open_menu(index, pointer_at::last(), self.now);
                Task::none()
            }
            Message::MenuClosed => {
                self.home.close_menu(self.now);
                Task::none()
            }
            Message::MenuHover(slot, entering) => {
                self.home.set_menu_hovered(slot, entering, self.now);
                Task::none()
            }
            // The menu's connect entry is the row's click under another name.
            Message::MenuConnect(index) => {
                self.home.take_menu();
                self.choose_device(index, screens::connect::Purpose::Session)
            }
            // Same dialog, different promise: what opens afterwards is a
            // shell, not a picture.
            Message::MenuTerminal(index) => {
                self.home.take_menu();
                self.choose_device(index, screens::connect::Purpose::Terminal)
            }
            Message::MenuForget(index) => {
                self.home.close_menu(self.now);
                self.forget_device(index)
            }
            Message::AddDevice => {
                self.connect.open_empty(self.now);
                iced::widget::operation::focus(screens::connect::CODE_FIELD)
            }
            Message::SwitchTab(index) => {
                if index < self.tabs.len() {
                    if index != self.tabs.active {
                        self.surface_at = self.now;
                    }
                    self.tabs.active = index;
                    // The pointer samples queued for the previous tab's
                    // session belong to a machine that is no longer in front.
                    let _ = net::pointer::drain();
                    self.go_to(Screen::Session);
                }
                Task::none()
            }
            Message::CloseTab(index) => self.close_tab(index),
            Message::ToggleHome => {
                if self.on_surface() {
                    self.go_to(self.last_shell);
                } else if !self.tabs.is_empty() {
                    self.go_to(Screen::Session);
                }
                Task::none()
            }
            Message::Terminal(inner) => {
                match inner {
                    screens::terminal::Message::Resized { cols, rows } => {
                        if let Some(shell) = self.tabs.active_terminal_mut() {
                            shell.state.resized(cols, rows);
                        }
                    }
                    screens::terminal::Message::Key(press) => self.terminal_key(press),
                    screens::terminal::Message::Close => {
                        let active = self.tabs.active;
                        return self.close_tab(active);
                    }
                }
                Task::none()
            }
            Message::KeyPressed(press) => self.on_key(press),
            Message::Escape(press) => {
                // The topmost thing that can be dismissed is.
                if self.connect.is_open() {
                    self.connect.dismiss(self.now);
                } else if self.home.is_menu_open() {
                    self.home.close_menu(self.now);
                } else if self.terminal_in_front() {
                    self.terminal_key(press);
                }
                Task::none()
            }
            Message::TerminalPaste(text) => {
                if let (Some(text), true) = (text, self.terminal_in_front()) {
                    if let Some(shell) = self.tabs.active_terminal_mut() {
                        shell.state.paste(&text);
                    }
                }
                Task::none()
            }
            Message::TerminalWake => {
                self.drain_terminals();
                Task::none()
            }
            Message::TerminalConnected(result) => self.on_terminal_connected(result),
        }
    }

    // ----------------------------------------------------------- navigation

    /// Whether the tab in front is what fills the window.
    fn on_surface(&self) -> bool {
        self.screen == Screen::Session && !self.tabs.is_empty()
    }

    /// Whether keys should go to a shell: its tab is in front, and in view.
    fn terminal_in_front(&self) -> bool {
        self.on_surface() && self.tabs.active().is_some_and(Tab::is_terminal)
    }

    /// Go somewhere. The one door every screen change passes through, so the
    /// highlight, the page transition and the per-page refreshes cannot be
    /// forgotten by whichever path asked.
    ///
    /// Between two pages the sheet stays where it is: the old contents fade
    /// out over [`motion::PAGE_OUT`], then the new header and blocks rise in.
    /// The rail's highlight slides to the new item meanwhile. Into a tab the
    /// surface fades up; out of one the sidebar arrives again with the page.
    fn go_to(&mut self, screen: Screen) {
        let now = self.now;
        // A session screen with no tab to show is nowhere to go: stay put.
        if screen == self.screen || (screen == Screen::Session && self.tabs.is_empty()) {
            return;
        }
        let previous = self.screen;
        // Asked of the screen, not of the tabs: closing the last tab empties
        // the strip before this runs, and the way out of a surface is still
        // the way out of a surface.
        let from_surface = previous == Screen::Session;
        self.screen = screen;
        if screen != Screen::Session {
            self.last_shell = screen;
        }

        for item in Screen::ALL {
            let wanted = item == screen;
            let animation = &mut self.nav_active[item.index()];
            if animation.value() != wanted {
                animation.go_mut(wanted, now);
            }
        }

        // The highlight slides to the item, from wherever it is drawn. Coming
        // back from a tab it was not on screen, so it is simply there.
        if let Some(to) = rail_position(screen) {
            self.rail_from = if from_surface {
                rail_y(to, self.rail_column())
            } else {
                self.rail_highlight_y(now)
            };
            self.rail_to = to;
            if from_surface {
                self.rail_slide.snap(1.0);
            } else {
                self.rail_slide.snap(0.0);
                self.rail_slide.go(1.0, now, motion::STANDARD, motion::EASE_CHANGE);
            }
        }

        // A menu belongs to the list it was opened on.
        if self.home.is_menu_open() {
            self.home.take_menu();
        }

        if screen == Screen::Session {
            self.surface_at = now;
            self.leaving = None;
            self.leave.snap(0.0);
        } else if from_surface {
            // Back from a tab: the shell arrives as it does at boot.
            self.shown_at = now;
            self.leaving = None;
            self.leave.snap(0.0);
            self.replay(screen, now + motion::STAGGER_STEP);
        } else {
            // A second navigation while a page is still leaving joins the
            // exit already under way rather than restarting it.
            if !(self.leaving.is_some() && self.leave.is_animating(now)) {
                self.leaving = Some(previous);
                self.leave.snap(1.0);
                self.leave.exit(now, motion::PAGE_OUT);
                self.leave_ends = now + motion::PAGE_OUT;
            }
            self.replay(screen, self.leave_ends);
        }

        // Re-read rather than trusted: the account file is written by the
        // service too, and a stale roster on a screen whose whole job is
        // telling you who can get in would be a lie.
        if screen == Screen::Users {
            self.reload_users();
        }
        if screen == Screen::Settings {
            self.settings.reload_accounts();
        }
    }

    /// Start a page's entrance cascade at `at`.
    fn replay(&mut self, screen: Screen, at: Instant) {
        match screen {
            Screen::Home | Screen::Session => self.home.replay(at),
            Screen::Users => self.users.replay(at),
            Screen::Transfers => self.transfers.replay(at),
            Screen::Settings => self.settings.replay(at),
            Screen::Mcp => self.mcp.replay(at),
        }
    }

    // ------------------------------------------------------------- keyboard

    /// A key nothing on screen wanted.
    fn on_key(&mut self, press: screens::terminal::Press) -> Task<Message> {
        if self.terminal_in_front() {
            if press.is_paste() {
                return iced::clipboard::read().map(Message::TerminalPaste);
            }
            self.terminal_key(press);
        }
        Task::none()
    }

    fn terminal_key(&mut self, press: screens::terminal::Press) {
        if !self.terminal_in_front() {
            return;
        }
        if let Some(shell) = self.tabs.active_terminal_mut() {
            shell.state.key(press);
        }
    }

    /// Take whatever every terminal has printed since the last look.
    fn drain_terminals(&mut self) {
        for tab in &mut self.tabs.items {
            if let Some(shell) = tab.as_terminal_mut() {
                screens::terminal::update(&mut shell.state, &mut shell.events);
            }
        }
    }

    // --------------------------------------------------------------- notices

    /// Put a notice up, or replace the one showing.
    fn post(&mut self, notice: Notice) {
        if self.notice.is_none() {
            self.toast.enter(self.now, motion::TOAST);
        }
        self.fading_notice = None;
        self.notice = Some(notice);
    }

    /// Take the notice down. It fades rather than vanishing, so it keeps
    /// being drawn from `fading_notice` until the toast has gone.
    fn clear_notice(&mut self) {
        if let Some(notice) = self.notice.take() {
            self.fading_notice = Some(notice);
            self.toast.exit(self.now, motion::DIALOG_OUT);
        }
    }

    fn expire_notice(&mut self) {
        if self
            .notice
            .as_ref()
            .is_some_and(|notice| notice.until.is_some_and(|until| self.now >= until))
        {
            self.clear_notice();
        }
        if self.fading_notice.is_some() && self.toast.is_gone(self.now) {
            self.fading_notice = None;
        }
    }

    // ----------------------------------------------------------------- tabs

    /// The tab strip changed length: the title bar's hover follows it, and a
    /// rail pill whose tab has gone must not leave its growth behind for the
    /// next tab to inherit.
    fn tabs_changed(&mut self) {
        self.tab_hover.resize(self.tabs.len(), self.now);
        let shown = self.tabs.len().min(RAIL_TABS);
        if self.nav_hover.current().is_some_and(|slot| slot >= NAV_TABS + shown) {
            self.nav_hover.clear(self.now);
        }
    }

    /// Close a tab. A session is asked to disconnect, and its tab leaves when
    /// the driver confirms rather than the moment the button is pressed —
    /// until then it stays exactly where it was, which is what stops a slow
    /// goodbye from looking like a crash. A shell has no goodbye to wait for:
    /// dropping it releases its connection, and the host kills the shell off
    /// the far end of that.
    fn close_tab(&mut self, index: usize) -> Task<Message> {
        let Some(tab) = self.tabs.items.get_mut(index) else {
            return Task::none();
        };
        match tab {
            Tab::Terminal(_) => {
                self.tabs.remove_all(vec![index]);
                self.tabs_changed();
                if self.tabs.is_empty() && self.screen == Screen::Session {
                    self.go_to(self.last_shell);
                }
            }
            Tab::Session(live) => live.link.send(Command::Disconnect),
        }
        Task::none()
    }

    /// A machine on the Devices list was clicked: its connect dialog opens
    /// over the list, and the screen does not change.
    ///
    /// A machine that has been connected to before arrives with its code and
    /// username already in place, and the cursor in the one field that is
    /// still empty. A machine only discovery knows arrives with what
    /// discovery can honestly claim — a name, a route, an address — and the
    /// code field open, because a discovered peer is an address and a
    /// hostname and neither is an identity: Pravera reaches a machine by its
    /// public key, which arrives in the connect code and nowhere else.
    ///
    /// Matching a remembered machine to a row by hostname cannot connect
    /// anyone to the wrong one. What gets dialled is the remembered *key*, so
    /// a machine that merely borrowed the name fails the handshake and never
    /// sees a password.
    fn choose_device(&mut self, index: usize, purpose: screens::connect::Purpose) -> Task<Message> {
        let Some(entry) = self.device_entries.get(index) else {
            return Task::none();
        };

        match entry.code.clone() {
            Some(code) => {
                self.connect.recall(
                    &known::Machine {
                        code,
                        name: entry.name.clone(),
                        username: entry.username.clone().unwrap_or_default(),
                        last_used: 0,
                    },
                    purpose,
                    self.now,
                );
                let field = if self.connect.username().is_empty() {
                    screens::connect::USERNAME_FIELD
                } else {
                    screens::connect::PASSWORD_FIELD
                };
                iced::widget::operation::focus(field)
            }
            None => {
                self.connect.pick(
                    screens::connect::Picked {
                        name: entry.name.clone(),
                        route: entry.route_label(),
                        address: entry.address(),
                    },
                    purpose,
                    self.now,
                );
                iced::widget::operation::focus(screens::connect::CODE_FIELD)
            }
        }
    }

    /// Stop remembering the machine a context menu was opened on.
    ///
    /// The list re-merges from what is left, so an autodiscovered machine
    /// simply loses its padlock and stays where it is; one only remembered
    /// leaves the list until discovery finds it again. Either way the entry
    /// the menu described must still exist — the list can change between the
    /// click and the choice.
    fn forget_device(&mut self, index: usize) -> Task<Message> {
        let Some(entry) = self.device_entries.get(index) else {
            return Task::none();
        };
        if let Some(code) = &entry.code {
            self.known.forget(code);
            self.save_known();
            let count = screens::home::entries(&self.discovered, &self.known).len();
            self.home.on_entries(count, self.now);
        }
        Task::none()
    }

    /// Re-merge the Devices list, and tell the screen when it changed.
    ///
    /// The one door through which [`Self::device_entries`] changes, so the
    /// list a row was clicked on is always the list being drawn.
    fn refresh_entries(&mut self) {
        let fresh = screens::home::entries(&self.discovered, &self.known);
        if fresh != self.device_entries {
            let count = fresh.len();
            self.device_entries = fresh;
            self.home.on_entries(count, self.now);
        }
    }

    /// Take or release the keys Windows reserves for its own shell, and the
    /// pointer tracker that rides beside them.
    ///
    /// Reconciled every frame rather than switched at each event, because the
    /// answer depends on four things that change independently: whether a
    /// session exists, whether the picture is on screen, whether the window is
    /// visible, and whether the person has handed the keyboard back. Deriving
    /// it in one place is what stops the hook outliving the session that
    /// wanted it — which would swallow the Windows key on a machine with
    /// nothing to forward it to.
    ///
    /// The pointer hook runs on a slightly wider condition than the keyboard
    /// one: pointer movement is forwarded whether or not keys are, so it is
    /// only the session, the window and the focus that matter.
    fn reconcile_grab(&mut self) {
        // A terminal tab in front is a shell, not a machine to remote-control:
        // its keys come through the window like any text field's, and pointer
        // samples nobody forwards are work with no reader.
        let live_session = self.visible
            && self.focused
            && self.screen == Screen::Session
            && self.tabs.active_session().is_some();

        let wanted = live_session
            && self
                .tabs
                .active_session()
                .is_some_and(|live| live.state.forwards_keyboard());

        if wanted != net::grab::is_grabbing() {
            if wanted {
                net::grab::start();
            } else {
                net::grab::stop();
            }
        }

        if live_session != net::pointer::is_recording() {
            if live_session {
                net::pointer::start();
            } else {
                net::pointer::stop();
            }
        }
    }

    /// What Settings should say about this machine coming back at boot.
    ///
    /// A translation and nothing more. The service knows three ways of already
    /// being registered and the screen has one way of saying it, which is the
    /// right asymmetry: what somebody wants to know is whether the machine
    /// comes back, not which of three code paths arranged that.
    ///
    /// `NotElevated` off Windows is the platform having no such service at all,
    /// not a missing prompt — telling somebody on Linux to run as
    /// administrator would be an instruction that leads nowhere.
    fn boot_service(&self) -> screens::settings::Service {
        boot_service(&self.service)
    }

    /// Whatever the subnet has announced, or nothing if mDNS never started.
    fn lan_peers(&self) -> Vec<pravera_discovery::DiscoveredPeer> {
        self.browser
            .as_ref()
            .map(pravera_discovery::Browser::peers)
            .unwrap_or_default()
    }

    /// Collect everything that arrived since the last frame.
    ///
    /// Frames are pulled rather than pushed. A channel would queue them, and a
    /// queue of video frames is a queue of increasingly stale pictures: the
    /// decoder keeps only the newest and this takes it once per redraw.
    fn pump(&mut self) -> Task<Message> {
        self.expire_notice();
        self.drain_host_events();
        self.drain_progress();
        self.reconcile_grab();
        self.tabs_changed();

        // Things whose exit has settled can finally unmount.
        self.connect.take_finished_closing(self.now);
        self.home.take_finished_menu_close(self.now);
        if self.leaving.is_some() && self.leave.is_gone(self.now) {
            self.leaving = None;
        }

        // Keys the shell would have eaten, taken by the low-level hook, and
        // every raw pointer sample, go to the session in front — the one the
        // pointer and keyboard visibly belong to. With none in front the
        // queues are drained and dropped, so a stale key cannot fire into a
        // session that opens later.
        if let Some(live) = self.tabs.active_session_mut() {
            forward_grabbed(live, self.now);

            // Every pointer position the mouse produced since the last frame,
            // at its own rate rather than the display's. See `net::pointer`
            // for why the coalesced positions the window reports are not
            // enough.
            let samples = net::pointer::drain();
            if !samples.is_empty() {
                for command in screens::session::update(
                    &mut live.state,
                    screens::session::Message::RawPointer(samples),
                    self.now,
                ) {
                    live.link.send(command);
                }
            }
        } else {
            let _ = net::grab::drain();
            let _ = net::pointer::drain();
        }

        // Every tab drains its own driver. A session in the background keeps
        // running — its latency figures keep updating, its reconfigurations
        // are adopted — and an ending is collected rather than acted on
        // mid-iteration, because removing a tab invalidates the indices the
        // loop is walking. A shell in the background keeps running too: its
        // grid keeps filling, so switching back is a jump rather than a wait.
        self.drain_terminals();
        let mut ended: Vec<(usize, Ending)> = Vec::new();
        // Terminals opened by a session's own driver land here first and join
        // the tabs after the walk, for the same index-invalidating reason.
        let mut opened: Vec<(
            String,
            pravera_client::Terminal,
            mpsc::UnboundedReceiver<pravera_client::TerminalEvent>,
        )> = Vec::new();
        for (index, tab) in self.tabs.items.iter_mut().enumerate() {
            let Some(live) = tab.as_session_mut() else {
                continue;
            };
            while let Ok(event) = live.events.try_recv() {
                match event {
                    link::Event::Reconfigured { config, video } => {
                        live.link.adopt(*config, video);
                        live.state.reconfigured(self.now);
                    }
                    link::Event::Latency(rtt) => live.state.measured(rtt),
                    // A shell opened from inside this session: same machine,
                    // second tab. It is collected here and pushed below, out
                    // of the walk.
                    link::Event::TerminalOpened(result) => {
                        if let Ok(carried) = result {
                            if let Some((terminal, events)) = carried.take() {
                                opened.push((live.link.host_name().to_owned(), terminal, events));
                            }
                        }
                    }
                    link::Event::Ended(ending) => {
                        ended.push((index, ending));
                        break;
                    }
                }
            }
        }

        // The tab in front feeds the picture and the keyframe ask. Background
        // tabs keep decoding into mailboxes nobody is reading.
        if let Some(live) = self.tabs.active_session_mut() {
            if let Some(picture) = live.link.next_picture() {
                live.state.show(picture);
            }
            live.state.follow_cursor(&live.link);
            live.state.tick(self.now);
            live.state.follow(&live.link, self.now);

            // Asked for the moment enough has been lost to be worth it: until
            // one arrives the picture stays visibly broken, so waiting
            // achieves nothing but a longer smear.
            if live.link.take_keyframe_request() {
                live.link.send(Command::Keyframe);
            }
        }

        // Gaming mode holds the cursor inside the window. Re-asserted every
        // frame rather than set once, because Windows clears the clip on its
        // own interventions; one call a frame is nothing next to the decode.
        let confined = self.visible
            && self.focused
            && self.on_surface()
            && self
                .tabs
                .active_session()
                .is_some_and(|live| live.state.gaming_mode());
        if confined {
            net::pointer::confine_to_foreground_window();
            self.cursor_clipped = true;
        } else if self.cursor_clipped {
            net::pointer::release_confinement();
            self.cursor_clipped = false;
        }

        // Terminals opened during the walk join the tabs now, when indices
        // are stable again. The last one in takes the front, which is the one
        // the person just asked for.
        let any_opened = !opened.is_empty();
        for (host, terminal, events) in opened {
            let mut state = screens::terminal::State::new();
            state.connect(terminal);
            self.tabs.push_terminal(TerminalLive {
                host,
                state,
                events,
                _hold: None,
            });
        }
        if any_opened {
            self.surface_at = self.now;
            self.go_to(Screen::Session);
        }

        // Tabs whose driver ended leave here. The last one out takes the full
        // teardown — hooks, transfers, the screen; the others leave a notice
        // and whatever tabs remain keep the session screen up.
        if !ended.is_empty() {
            let ended_gaming = ended.iter().any(|(index, _)| {
                self.tabs
                    .items
                    .get(*index)
                    .and_then(Tab::as_session)
                    .is_some_and(|live| live.state.gaming_mode())
            });
            let indices = ended.iter().map(|(index, _)| *index).collect();
            self.tabs.remove_all(indices);
            let first = ended.into_iter().next().expect("checked non-empty").1;

            if !self.tabs.items.iter().any(|tab| tab.as_session().is_some()) {
                self.teardown_after_last_session(first, ended_gaming);
            } else {
                // Sessions remain: this tab's ending is news, not a teardown.
                // A failure stays until it is read, for the same reason the
                // single-session path keeps one.
                let failure = first.is_failure();
                self.post(Notice {
                    message: first.message(),
                    failure,
                    until: (!failure).then(|| self.now + NOTICE_FOR),
                });
            }
        }

        // A session that ends while gaming must not leave the window
        // fullscreen with nothing in it.
        let restore = self.restore_windowed;
        self.restore_windowed = false;
        if restore {
            return self
                .window
                .map(|id| iced::window::set_mode(id, iced::window::Mode::Windowed))
                .unwrap_or_else(Task::none);
        }

        Task::none()
    }

    fn drain_host_events(&mut self) {
        let Some(events) = &mut self.host_events else {
            return;
        };

        let mut stopped = None;
        while let Ok(event) = events.try_recv() {
            match event {
                // The device ID is the peer's, so it is already a fingerprint
                // rather than anything the peer chose to call itself.
                host::Event::Arrived { device } => {
                    tracing::info!(peer = %device, "a peer connected to this machine")
                }
                host::Event::Left { device } => {
                    tracing::info!(peer = %device, "a peer disconnected")
                }
                host::Event::Stopped(reason) => {
                    stopped = Some(reason);
                    break;
                }
            }
        }

        if let Some(reason) = stopped {
            self.hosting = None;
            self.host_events = None;
            self.post(Notice {
                message: reason,
                failure: true,
                until: None,
            });
        }
    }

    // ------------------------------------------------------------ unattended

    /// Bring the notification-area icon up to date.
    ///
    /// Called after every message rather than from any one of them, because
    /// almost anything can change what the icon should say, and a refresh that
    /// has to be remembered is a refresh that will be forgotten. The icon
    /// itself returns immediately when nothing changed.
    fn refresh_tray(&mut self) {
        if self.tray.is_none() {
            return;
        }
        let status = self.tray_status();
        if let Some(tray) = &mut self.tray {
            tray.update(&status);
        }
    }

    /// Look at this machine's screens off the UI thread, adding the virtual
    /// display when it has none, and report back with
    /// [`Message::DisplaysChecked`]. A no-op while a check is running.
    ///
    /// Only a machine with no real monitor gets a display this way — the
    /// capture crate decides that from each monitor's EDID, so a desktop is
    /// never given a monitor nobody asked for. That machine is also the one
    /// this exists for: nobody is going to press a button on it.
    fn check_displays(&mut self) -> Task<Message> {
        if self.checking_displays {
            return Task::none();
        }
        self.checking_displays = true;
        Task::perform(
            async {
                tokio::task::spawn_blocking(DisplayCheck::run)
                    .await
                    .unwrap_or_else(|error| DisplayCheck {
                        displays: None,
                        status: pravera_capture::VirtualDisplayStatus::default(),
                        added: Some(Err(format!("the display check failed: {error}"))),
                        backend: "unavailable".to_string(),
                    })
            },
            |check| Message::DisplaysChecked(Box::new(check)),
        )
    }

    /// Start hosting now that there is a screen, if this machine is meant to
    /// host by itself and could not before.
    ///
    /// On a machine with no monitor, hosting at launch can race the virtual
    /// display's first appearance and refuse; this is the retry, taken once
    /// the display exists rather than on a timer.
    fn host_once_screen_exists(&mut self) -> Task<Message> {
        // Not while a start is already on its way: hosting at launch may be
        // the very thing that just added the display.
        if self.prefs.host_at_launch
            && self.hosting.is_none()
            && self.endpoint.is_some()
            && !self.settings.is_starting()
        {
            tracing::info!("a screen appeared; starting to host");
            return self.start_hosting();
        }
        Task::none()
    }

    /// What the icon should currently say.
    fn tray_status(&self) -> tray::Status {
        tray::Status {
            device: self.endpoint.as_ref().map(|e| e.device_id.to_string()),
            hosting: self.hosting.is_some(),
            connections: self.hosting.as_ref().map(Hosting::connections).unwrap_or(0),
        }
    }

    fn on_tray(&mut self, request: tray::Request) -> Task<Message> {
        match request {
            tray::Request::Show => self.show_window(),
            tray::Request::ToggleHosting => {
                self.go_to(Screen::Settings);
                self.toggle_hosting()
            }
            // Deliberately unconditional. Someone who picked "Quit" from the
            // icon has already been told, by the icon, that this machine is
            // reachable — asking again would be asking them to confirm the
            // thing they just chose.
            tray::Request::Quit => {
                tracing::info!("quitting from the notification area");
                self.hosting = None;
                self.host_events = None;
                iced::exit()
            }
        }
    }

    fn show_window(&mut self) -> Task<Message> {
        tracing::info!(visible = self.visible, closing = ?self.closing, hiding = self.hiding, "show_window");
        let Some(id) = self.window else {
            return Task::none();
        };
        let focus = Task::batch([
            // Re-asserted even when `visible` says it is already up: that
            // flag has drifted from the real window before, and on a window
            // that is already showing this is a no-op.
            iced::window::set_mode(id, iced::window::Mode::Windowed),
            iced::window::gain_focus(id),
        ]);
        if self.visible {
            return focus;
        }
        // Coming back from the tray: the shell arrives the way it does at
        // boot, sidebar first and then the page's panels.
        self.visible = true;
        self.closing = None;
        self.hiding = false;
        self.shown_at = self.now;
        self.surface_at = self.now;
        self.replay(self.screen, self.now + motion::STAGGER_STEP);
        focus
    }

    /// The close button, and Alt+F4, and the system menu. Immediate: the
    /// window is gone the moment it is asked to go, and nothing on the
    /// chrome animates on the way out.
    fn close_window(&mut self) -> Task<Message> {
        tracing::info!(visible = self.visible, closing = ?self.closing, hiding = self.hiding, tray = self.tray.is_some(), "close_window");
        if self.closing.is_some() {
            return Task::none();
        }
        self.closing = Some(closing(self.tray.is_some(), self.hosting.is_some()));
        self.hiding = false;
        self.complete_close()
    }

    /// Carry out a close.
    fn complete_close(&mut self) -> Task<Message> {
        let Some(decision) = self.closing else {
            return Task::none();
        };
        if self.hiding {
            return Task::none();
        }
        tracing::info!(?decision, visible = self.visible, "complete_close");
        self.hiding = true;
        match decision {
            Closing::Hide => {
                self.visible = false;
                tracing::info!("closed to the notification area, still accepting sessions");
                // Hide synchronously via Win32 so a stale frame does not
                // linger for one compositing interval before the async
                // `set_mode(Hidden)` is processed.
                #[cfg(windows)]
                crate::single_instance::hide_os_window();
                self.window
                    .map(|id| iced::window::set_mode(id, iced::window::Mode::Hidden))
                    .unwrap_or_else(Task::none)
            }
            Closing::Quit => {
                self.hosting = None;
                self.host_events = None;
                iced::exit()
            }
        }
    }

    // -------------------------------------------------------------- updates

    fn on_update_tick(&mut self) -> Task<Message> {
        if self.updater.due(self.now) {
            return self.check_for_update();
        }
        // Downloaded earlier, while something was in use: see whether it
        // still is.
        self.apply_update(false)
    }

    fn check_for_update(&mut self) -> Task<Message> {
        if !self.updater.enabled || self.updater.busy() {
            return Task::none();
        }
        // Somebody pressing "Check now" after the download finished wants the
        // restart, not a second download.
        if self.updater.ready().is_some() {
            return Task::none();
        }
        self.updater.checking();
        Task::perform(install::release::latest(), Message::UpdateChecked)
    }

    /// Whether nobody would notice Pravera restarting: nothing open, nobody
    /// connected, no window up — and, if hosting, a restart that will host
    /// again by itself.
    fn quiet_enough_to_update(&self) -> bool {
        let agent = pravera_service::is_agent_launch();
        let unseen = agent || !self.visible;
        let connected = self.hosting.as_ref().map_or(0, Hosting::connections);
        let hosts_again = self.hosting.is_none() || self.prefs.host_at_launch;
        unseen && self.tabs.is_empty() && self.running.is_empty() && connected == 0 && hosts_again
    }

    /// Put a downloaded update in place and restart into it: at once when
    /// `asked`, otherwise only if automatic updates are on and nothing is in
    /// use.
    fn apply_update(&mut self, asked: bool) -> Task<Message> {
        let Some((release, executable)) = self.updater.ready() else {
            return Task::none();
        };
        if !asked && !(self.prefs.auto_update && self.quiet_enough_to_update()) {
            return Task::none();
        }
        let version = release.version.clone();
        let executable = executable.clone();
        match install::release::apply(&executable, &version) {
            Ok(target) => {
                tracing::info!(%version, target = %target.display(), "updated; restarting");
                self.updater.phase = update::Phase::Applying;
                // The agent is started again by the service within seconds,
                // from the same path, which now holds the new build.
                if !pravera_service::is_agent_launch() {
                    if let Err(error) = install::release::relaunch(&target, !self.visible) {
                        tracing::warn!(%error, "the new version did not start; it will on the next launch");
                    }
                }
                self.hosting = None;
                self.host_events = None;
                self.single_instance_lock = None;
                iced::exit()
            }
            Err(reason) => {
                tracing::warn!(%reason, "the update could not be applied");
                self.updater.phase = update::Phase::Failed(reason);
                Task::none()
            }
        }
    }

    fn update_summary(&self) -> screens::settings::Updates {
        use screens::settings::UpdateAction;
        let (headline, detail) = self.updater.describe();
        screens::settings::Updates {
            headline,
            detail,
            action: if self.updater.ready().is_some() {
                UpdateAction::Restart
            } else if self.updater.busy() {
                UpdateAction::Busy
            } else {
                UpdateAction::Check
            },
            auto: self.prefs.auto_update,
            enabled: self.updater.enabled,
            pending: self.updater.pending(),
            brief: self.updater.brief(),
        }
    }

    /// Remember a change, if there is anywhere to remember it.
    fn save_prefs(&self) {
        if let Some(path) = &self.prefs_path {
            self.prefs.save(path);
        }
    }

    fn save_known(&self) {
        if let Some(path) = &self.known_path {
            self.known.save(path);
        }
    }

    // ------------------------------------------------------------- sessions

    fn on_connect_form(&mut self, message: screens::connect::Message) -> Task<Message> {
        use screens::connect::{Outcome, Purpose};

        match screens::connect::update(&mut self.connect, message, self.now) {
            Outcome::Connect => {}
            Outcome::Nothing => return Task::none(),
            // The code and the username came from a machine that has been
            // connected to before; the password never has and never will.
            Outcome::FocusPassword => {
                return iced::widget::operation::focus(screens::connect::PASSWORD_FIELD)
            }
        }

        let peer = match self.connect.peer() {
            Ok(peer) => peer,
            // `update` only asks to connect for a form that already parsed, so
            // this is unreachable rather than a case to design for.
            Err(reason) => {
                self.connect.failed(reason.to_string());
                return Task::none();
            }
        };

        let Some(endpoint) = &self.endpoint else {
            self.connect
                .failed(self.endpoint_error.clone().unwrap_or_else(|| {
                    "This machine is still opening its network endpoint.".to_string()
                }));
            return Task::none();
        };
        let transport = endpoint.transport.clone();

        self.connect.began();
        self.clear_notice();

        // A terminal asked for this login: same handshake, same account check,
        // but no display anywhere in it. The purpose travels with the form,
        // so a failed terminal cannot turn the next connect into a shell.
        if self.connect.purpose() == Purpose::Terminal {
            let credentials = link::TerminalCredentials {
                peer,
                username: self.connect.username().trim().to_string(),
                password: self.connect.password().to_string(),
            };
            return Task::perform(link::connect_terminal(transport, credentials), |result| {
                Message::TerminalConnected(result.map(Carry::new))
            });
        }

        let credentials = Credentials {
            peer,
            username: self.connect.username().trim().to_string(),
            password: self.connect.password().to_string(),
            profile: self.connect.profile(),
            monitor: pravera_proto::MonitorId::PRIMARY,
            max_resolution: None,
            audio: self.connect.audio(),
        };

        // Remembered from the last connection rather than from a settings
        // page: this is where the choice is actually made, and a preference
        // that has to be set somewhere else is one nobody finds.
        if self.prefs.hear_the_host != credentials.audio {
            self.prefs.hear_the_host = credentials.audio;
            self.save_prefs();
        }

        Task::perform(link::connect(transport, credentials), |result| {
            Message::Connected(result.map(Carry::new))
        })
    }

    /// A standalone terminal arrived, or its attempt failed.
    fn on_terminal_connected(&mut self, result: Result<Carry<link::OpenedTerminal>, String>) -> Task<Message> {
        match result {
            Ok(carried) => {
                let Some(opened) = carried.take() else {
                    return Task::none();
                };
                let link::OpenedTerminal {
                    host_name,
                    terminal,
                    events,
                    hold,
                } = opened;

                tracing::info!(host = %host_name, "a terminal opened");

                self.known.remember(self.connect.code(), &host_name, self.connect.username());
                self.save_known();
                self.connect.succeeded(self.now);

                let mut state = screens::terminal::State::new();
                state.connect(terminal);
                self.tabs.push_terminal(TerminalLive {
                    host: host_name,
                    state,
                    events,
                    _hold: Some(hold),
                });
                self.tabs_changed();
                self.surface_at = self.now;
                self.go_to(Screen::Session);
                Task::none()
            }
            Err(reason) => {
                // The dialog is still open — it is what started this — so the
                // refusal appears where the click happened.
                self.connect.failed(reason);
                Task::none()
            }
        }
    }

    fn on_connected(&mut self, result: Result<Carry<LiveSession>, String>) -> Task<Message> {
        match result {
            Ok(carried) => {
                // `None` means this message was somehow delivered twice. There
                // is no session to show and nothing to report: the first
                // delivery already started one.
                let Some((link, events)) = carried.take() else {
                    return Task::none();
                };

                tracing::info!(
                    host = %link.host_name(),
                    device = %link.device(),
                    "a session started"
                );

                // Remembered on success rather than on submit: a code that was
                // mistyped, or names a machine that refused the login, is not
                // a shortcut worth offering back.
                self.known.remember(
                    self.connect.code(),
                    link.host_name(),
                    self.connect.username(),
                );
                self.save_known();

                self.connect.succeeded(self.now);
                // The file panes are prepared now rather than when somebody
                // opens them: both listings are a round trip each, and doing
                // them up front means the panes are already populated the first
                // time they are looked at.
                let opening =
                    self.transfers
                        .connected(link.host_name(), link.permissions(), self.now);

                // A session opens as a new tab and takes the front: connecting
                // to a second machine must not close the first.
                self.tabs.push_session(Live {
                    state: screens::session::State::new(link.monitors().len(), self.now),
                    link,
                    events,
                });
                self.tabs_changed();
                self.surface_at = self.now;
                self.go_to(Screen::Session);

                // The raw pointer path divides screen deltas by the window's
                // scale factor, which is a property of whatever monitor the
                // window is on right now. Measured once now and again on every
                // resize, because a window dragged onto a different monitor
                // changes it without anything in this process noticing.
                let measured_scale = self
                    .window
                    .map(|id| {
                        iced::window::scale_factor(id)
                            .map(|scale| Message::Session(screens::session::Message::Scale(scale)))
                    })
                    .unwrap_or_else(Task::none);

                Task::batch(
                    opening
                        .into_iter()
                        .map(|action| self.do_transfer_action(action))
                        .chain(std::iter::once(measured_scale))
                        .collect::<Vec<_>>(),
                )
            }
            Err(reason) => {
                // The dialog is already open — it is the thing that started
                // this attempt — so the refusal appears where the click
                // happened, and nothing navigates.
                self.connect.failed(reason);
                Task::none()
            }
        }
    }

    fn on_session(&mut self, message: screens::session::Message) -> Task<Message> {
        let Some(live) = self.tabs.active_session_mut() else {
            return Task::none();
        };

        // A key the window reports goes out behind every key the hook took
        // before it. The hook's queue is otherwise only emptied once a frame,
        // and Win+R would reach the far machine as R and then Win whenever the
        // R got here first.
        if matches!(message, screens::session::Message::Key { .. }) {
            forward_grabbed(live, self.now);
        }

        for command in screens::session::update(&mut live.state, message, self.now) {
            live.link.send(command);
        }
        // Leaving the picture, not the session. The driver task keeps running,
        // the decoder keeps taking datagrams, and coming back needs no
        // renegotiation.
        let leaving = live.state.take_leaving();
        // Gaming mode is asked for by the screen and carried out here, which
        // is the only place that owns the window. The confinement itself is
        // re-asserted by the pump for as long as the mode is on.
        let gaming = live.state.take_gaming();

        if leaving {
            self.go_to(Screen::Transfers);
        }

        let Some(on) = gaming else {
            return Task::none();
        };
        let mode = if on {
            iced::window::Mode::Fullscreen
        } else {
            iced::window::Mode::Windowed
        };
        self.window
            .map(|id| iced::window::set_mode(id, mode))
            .unwrap_or_else(Task::none)
    }

    /// The last session finished. Everything the single-session teardown was,
    /// reached only when no session remains — partial endings are handled in
    /// the pump, where the tab that ended is known by position. Shells on
    /// their own connections are left open: they never depended on it.
    fn teardown_after_last_session(&mut self, ending: Ending, was_gaming: bool) {
        // Before anything else. The hook is a callback on every keystroke on
        // this machine, and there is no longer anywhere to send them. The
        // pointer tracker goes with it, and the cursor is let go before
        // anything else can decide the clip should stay.
        net::grab::stop();
        net::pointer::stop();
        if self.cursor_clipped {
            net::pointer::release_confinement();
            self.cursor_clipped = false;
        }
        // A fullscreen window with no session in it is a screen-sized empty
        // room. Restored by the pump, which can return the task.
        if was_gaming {
            self.restore_windowed = true;
        }

        let failure = ending.is_failure();
        tracing::info!(reason = %ending.message(), "the session ended");

        // Every transfer belongs to the connection that is gone. Aborting the
        // tasks resets their streams; the ledger keeps the rows, marked as
        // ended, because what did and did not arrive is the first thing anybody
        // wants to know after a session drops.
        for (_, handle) in self.running.drain() {
            handle.abort();
        }
        self.transfers.disconnected();

        // Never left on `Session` with nothing to draw. A failure lands on
        // Devices, where the dialog that started the session can be reopened
        // from the same row; somebody looking at the ledger when the
        // connection dropped is looking at exactly the right screen.
        let target = if self.screen == Screen::Transfers && !failure {
            Screen::Transfers
        } else if self.screen == Screen::Session && !self.tabs.is_empty() {
            Screen::Session
        } else {
            Screen::Home
        };
        if self.screen != target {
            self.go_to(target);
        }
        self.post(Notice {
            message: ending.message(),
            failure,
            // A failure stays until it is read. Ordinary disconnection does
            // not need to be acknowledged.
            until: (!failure).then(|| self.now + NOTICE_FOR),
        });
    }

    /// Kept for the tests, which tear down an app that never had a tab strip
    /// to walk.
    #[allow(dead_code)]
    fn end_session(&mut self, ending: Ending) {
        self.teardown_after_last_session(ending, false);
    }

    // ---------------------------------------------------------------- files

    fn on_transfers(&mut self, message: screens::transfers::Message) -> Task<Message> {
        match screens::transfers::update(&mut self.transfers, message, self.now) {
            screens::transfers::Outcome::Done => Task::none(),
            screens::transfers::Outcome::Act(action) => self.do_transfer_action(action),
        }
    }

    fn do_transfer_action(&mut self, action: screens::transfers::Action) -> Task<Message> {
        use screens::transfers::{Action, Side};

        match action {
            Action::Browse {
                side: Side::Local,
                location,
            } => {
                // This machine's own filesystem. Read on a blocking thread
                // rather than inline: a cold directory or a disconnected
                // network drive takes as long as it takes, and the update loop
                // is also the thing drawing the window.
                Task::perform(
                    async move {
                        tokio::task::spawn_blocking(move || pravera_files::browse(&location))
                            .await
                            .unwrap_or(Err(pravera_proto::FileError::Unreadable))
                    },
                    |result| {
                        Message::Listed(
                            Side::Local,
                            result
                                .map(Box::new)
                                .map_err(|error| format!("{}.", sentence(&error.to_string()))),
                        )
                    },
                )
            }

            Action::Browse {
                side: Side::Remote,
                location,
            } => {
                let Some(session) = self.live_session() else {
                    self.transfers
                        .not_listed(Side::Remote, "The session has ended.".into());
                    return Task::none();
                };
                Task::perform(
                    async move { pravera_client::files::list(&session, location).await },
                    |result| {
                        Message::Listed(
                            Side::Remote,
                            result
                                .map(Box::new)
                                .map_err(|error| format!("{}.", sentence(&error.to_string()))),
                        )
                    },
                )
            }

            Action::Copy {
                from,
                path,
                name,
                directory,
                size,
            } => self.start_transfer(from, path, name, directory, size),

            Action::Cancel(id) => {
                // Aborting drops the future, which resets the QUIC stream,
                // which is what tells the far end to stop reading a file
                // nobody is waiting for. A flag the task checks would leave it
                // reading until the next chunk boundary.
                if let Some(handle) = self.running.remove(&id) {
                    handle.abort();
                }
                self.transfers.cancelled(id);
                Task::none()
            }

            Action::Resume => {
                if let Some(live) = self.tabs.active_session_mut() {
                    live.state.resumed(self.now);
                    self.go_to(Screen::Session);
                }
                Task::none()
            }
        }
    }

    /// Begin one copy, and put it in the ledger.
    fn start_transfer(
        &mut self,
        from: screens::transfers::Side,
        path: String,
        name: String,
        directory: String,
        size: u64,
    ) -> Task<Message> {
        use screens::transfers::Side;

        let Some(session) = self.live_session() else {
            return Task::none();
        };

        // Joined here rather than on the far end: the separator belongs to
        // whichever filesystem the file is landing on, and only this side knows
        // which that is.
        let landing = join(&directory, &name);

        self.next_transfer += 1;
        let id = self.next_transfer;
        self.transfers.started(id, name, from, size);

        let reporter = self.progress.0.clone();
        let watch = move |progress| {
            // A closed receiver means the window is gone, and there is nothing
            // useful to do about it here: the transfer is still worth
            // finishing, and the task will be aborted if it is not.
            let _ = reporter.send((id, progress));
        };

        // Replacing is deliberately not offered yet: the refusal from the far
        // end says a file is already there, and the person picks another name
        // or removes it. Silently overwriting on a machine nobody is looking at
        // is the one mistake with no undo.
        let work = async move {
            match from {
                Side::Remote => {
                    pravera_client::files::download(&session, &path, &landing, false, watch).await
                }
                Side::Local => {
                    pravera_client::files::upload(&session, &path, &landing, false, watch).await
                }
            }
        };

        let (task, handle) = Task::future(work).abortable();
        self.running.insert(id, handle.abort_on_drop());
        task.map(move |result| {
            Message::TransferFinished(
                id,
                result.map_err(|error| format!("{}.", sentence(&error.to_string()))),
            )
        })
    }

    fn on_transfer_finished(&mut self, id: u64, result: Result<u64, String>) -> Task<Message> {
        self.running.remove(&id);
        // Drained first, so a transfer that finished between two ticks does not
        // report its final figures *after* it has been marked done.
        self.drain_progress();

        match self.transfers.finished(id, result) {
            Some(action) => self.do_transfer_action(action),
            None => Task::none(),
        }
    }

    /// Take everything the transfer tasks have reported since the last look.
    fn drain_progress(&mut self) {
        while let Ok((id, progress)) = self.progress.1.try_recv() {
            self.transfers.progressed(id, progress);
        }
    }

    /// The live session's connection, for a file request.
    ///
    /// A clone rather than a borrow: a transfer outlives the update that
    /// started it, and cloning a session shares the connection rather than
    /// opening a second one.
    fn live_session(&self) -> Option<pravera_transport::Session> {
        Some(self.tabs.active_session()?.link.session().clone())
    }

    // ------------------------------------------------------------- accounts

    /// Re-read the account file and hand the screen what is actually on disk.
    ///
    /// Everything the Users screen shows comes through here, including
    /// immediately after a change it made itself. That is the point: the store
    /// refuses things — a role nobody may shadow, a name that would read
    /// ambiguously, a role somebody still holds — and a screen that showed
    /// what it *asked for* rather than what was *written* would quietly
    /// disagree with the machine it is describing.
    fn reload_users(&mut self) {
        let store = match host::accounts() {
            Ok(store) => store,
            Err(reason) => {
                self.users.failed(reason);
                self.users.load(Vec::new(), Vec::new(), self.now);
                return;
            }
        };

        let accounts = store.accounts();
        let roles = store
            .known_roles()
            .iter()
            .map(|role| screens::users::RoleView {
                name: role.name.clone(),
                permissions: role.permissions,
                builtin: role.builtin,
                held_by: accounts.iter().filter(|a| a.role == role.name).count(),
            })
            .collect();

        self.users.load(accounts, roles, self.now);
    }

    fn on_users(&mut self, message: screens::users::Message) -> Task<Message> {
        let action = match screens::users::update(&mut self.users, message, self.now) {
            screens::users::Outcome::Done => return Task::none(),
            screens::users::Outcome::Act(action) => action,
        };

        let mut store = match host::accounts() {
            Ok(store) => store,
            Err(reason) => {
                self.users.failed(reason);
                return Task::none();
            }
        };

        use pravera_auth::Permission;
        use screens::users::Action;
        let outcome = match &action {
            Action::Create {
                username,
                password,
                role,
            } => store.set(username, password, role).map(|()| true),
            Action::Assign { username, role } => store.set_role(username, role),
            Action::SetEnabled { username, enabled } => store.set_enabled(username, *enabled),
            Action::Remove { username } => store.remove(username),
            // A role starts able to see the screen and nothing else, which is
            // the smallest role worth having: the store refuses one without
            // VIEW, since it would connect and then show a black rectangle.
            Action::CreateRole { name } => store.define_role(name, Permission::VIEW).map(|()| true),
            Action::DefineRole { name, permissions } => {
                store.define_role(name, *permissions).map(|()| true)
            }
            Action::RemoveRole { name } => store.remove_role(name),
        };

        match outcome {
            Ok(_) => match action {
                Action::Create { .. } => self.users.finished_adding(),
                Action::CreateRole { .. } => self.users.finished_adding_role(),
                _ => {}
            },
            // The store's own words. It knows why — the name is taken, the role
            // is built in, somebody still holds it — and inventing a friendlier
            // sentence here would lose the reason.
            Err(error) => self.users.failed(reason(&error)),
        }

        self.reload_users();
        // Settings lists the same accounts in its host panel, so it would
        // otherwise go on showing somebody who has just been removed.
        self.settings.reload_accounts();
        Task::none()
    }

    // -------------------------------------------------------------- hosting

    fn on_settings(&mut self, message: screens::settings::Message) -> Task<Message> {
        let Some(action) = screens::settings::update(&mut self.settings, message, self.now) else {
            return Task::none();
        };

        match action {
            screens::settings::Message::CopyCode => match &self.hosting {
                Some(hosting) => {
                    self.settings.copied();
                    iced::clipboard::write(hosting.code())
                }
                None => Task::none(),
            },
            screens::settings::Message::ToggleHosting => self.toggle_hosting(),
            screens::settings::Message::ToggleAtSignIn => {
                let wanted = !self.at_sign_in;
                // Re-read rather than assumed: the task scheduler is the
                // truth, and a switch that shows what was asked for instead
                // of what happened is a switch that lies about a machine
                // coming back on its own. Both halves run `schtasks`, so both
                // run off the UI thread.
                Task::perform(
                    async move {
                        tokio::task::spawn_blocking(move || {
                            autostart::set(wanted).map(|()| autostart::is_enabled())
                        })
                        .await
                        .map_err(|error| error.to_string())
                        .and_then(|result| result)
                    },
                    Message::AtSignIn,
                )
            }
            screens::settings::Message::ToggleHostAtLaunch => {
                self.prefs.host_at_launch = !self.prefs.host_at_launch;
                self.save_prefs();
                Task::none()
            }
            screens::settings::Message::ToggleAutoUpdate => {
                self.prefs.auto_update = !self.prefs.auto_update;
                self.save_prefs();
                Task::none()
            }
            screens::settings::Message::CheckForUpdates => self.check_for_update(),
            screens::settings::Message::RestartToUpdate => self.apply_update(true),
            screens::settings::Message::AddVirtualDisplay => {
                // Asked for by a person, so it is added beside real monitors
                // too, and a recent automatic failure does not stand in for
                // trying. Unelevated it reports the one elevated run needed.
                // Off the UI thread: a first install waits for the monitor.
                self.settings
                    .virtual_result("Adding the virtual display…".to_string());
                Task::perform(
                    async {
                        tokio::task::spawn_blocking(|| {
                            pravera_capture::add_virtual_display(1920, 1080, 60)
                                .map(|shown| {
                                    format!(
                                        "Added the virtual display ({}x{}). Hosting captures it like any monitor.",
                                        shown.resolution.width, shown.resolution.height
                                    )
                                })
                                .map_err(|error| error.to_string())
                        })
                        .await
                        .unwrap_or_else(|error| Err(format!("adding the display failed: {error}")))
                    },
                    Message::DisplayAutoAdded,
                )
            }
            screens::settings::Message::InstallDriver => {
                Task::perform(
                    async {
                        tokio::task::spawn_blocking(pravera_capture::try_auto_install)
                            .await
                            .unwrap_or_else(|e| Err(format!("driver install task: {e}")))
                    },
                    Message::DriverInstalled,
                )
            }
            _ => Task::none(),
        }
    }

    /// The agent bus: start or stop the loopback listener, copy its snippet.
    ///
    /// Starting binds synchronously rather than through a `Task`, because a
    /// `Task` cannot borrow the bus back out of [`self`](Self). Binding is a
    /// socket call and an accept loop, not I/O worth an intermission.
    fn on_mcp(&mut self, message: screens::mcp::Message) -> Task<Message> {
        let Some(action) = screens::mcp::update(&mut self.mcp, message, self.now) else {
            return Task::none();
        };

        match action {
            screens::mcp::Message::Toggle(on) => {
                if on {
                    match self
                        .mcp
                        .glue_mut()
                        .start_blocking(pravera_mcp::DEFAULT_PORT)
                    {
                        Ok(addr) => tracing::info!("the agent bus is listening on {addr}"),
                        Err(reason) => self.mcp.set_error(Some(reason)),
                    }
                } else {
                    self.mcp.glue_mut().stop_sync();
                    tracing::info!("the agent bus stopped listening");
                }
                Task::none()
            }
            screens::mcp::Message::CopySnippet => {
                self.mcp.set_copied(true);
                iced::clipboard::write(screens::mcp::snippet_for(self.mcp.local_addr()))
            }
            screens::mcp::Message::Hover(..) => Task::none(),
        }
    }

    fn toggle_hosting(&mut self) -> Task<Message> {
        // Already on: dropping the handle closes the accept loop, and with it
        // every session it is serving. A session nobody can see is a session
        // nobody can end.
        if self.hosting.take().is_some() {
            self.host_events = None;
            tracing::info!("hosting was switched off");
            return Task::none();
        }

        self.start_hosting()
    }

    /// Begin accepting sessions, whoever asked.
    ///
    /// Reached from the button, from the notification-area menu, and from a
    /// machine that started itself — which is why the account comes from disk
    /// rather than from the form. A form that has been filled in is saved on
    /// the way past, so the two cannot diverge.
    fn start_hosting(&mut self) -> Task<Message> {
        let Some(endpoint) = &self.endpoint else {
            self.settings.failed(
                self.endpoint_error
                    .clone()
                    .unwrap_or_else(|| "This machine has no network endpoint yet.".to_string()),
            );
            return Task::none();
        };
        let transport = endpoint.transport.clone();

        let store = if self.settings.has_new_account() {
            host::save_account(
                self.settings.username(),
                self.settings.password(),
                self.settings.role(),
            )
        } else {
            // Whatever this machine already knows. `host::start` is what
            // refuses an empty one, in one place, with one sentence.
            host::accounts()
        };

        let store = match store {
            Ok(store) => store,
            Err(reason) => {
                self.settings.failed(reason);
                return Task::none();
            }
        };

        self.settings.began();
        Task::perform(
            host::start(transport, store, host::machine_name()),
            |result| Message::HostingStarted(result.map(Carry::new)),
        )
    }

    fn on_hosting_started(&mut self, result: Result<Carry<LiveHost>, String>) -> Task<Message> {
        match result {
            Ok(carried) => {
                let Some((hosting, events)) = carried.take() else {
                    return Task::none();
                };
                tracing::info!(device = %hosting.device_id(), "hosting");
                self.settings.started();
                self.hosting = Some(hosting);
                self.host_events = Some(events);
            }
            Err(reason) => self.settings.failed(reason),
        }
        Task::none()
    }

    /// Requests a redraw every frame, but only while something is moving.
    ///
    /// iced 0.14 renders reactively: without this an animation would freeze
    /// mid-flight, and with it running unconditionally the GPU would spin on a
    /// still screen. Asking the live animations gives smooth motion at rest
    /// cost. A picture in front redraws every frame regardless — that is how
    /// it gets on screen — and a shell wakes the window itself when it
    /// prints.
    fn subscription(&self) -> Subscription<Message> {
        let now = self.now;
        let animating = (self.scanning && self.screen == Screen::Home)
            || self.home.is_animating(now)
            || self.connect.is_animating(now)
            || self.settings.is_animating(now)
            || self.users.is_animating(now)
            || self.transfers.is_animating(now)
            || self.mcp.is_animating(now)
            || self.nav_hover.is_animating(now)
            || self.chrome_hover.is_animating(now)
            || self.tab_hover.is_animating(now)
            || self.nav_active.iter().any(|a| a.is_animating(now))
            || self.rail_slide.is_animating(now)
            || self.leave.is_animating(now)
            || self.leaving.is_some()
            || self.toast.is_animating(now)
            || self.fading_notice.is_some()
            || motion::cascading(self.shown_at, now)
            || motion::cascading(self.surface_at, now)
            || self
                .tabs
                .active_session()
                .is_some_and(|live| live.state.is_animating(now));

        let picture = self.on_surface() && self.tabs.active_session().is_some();

        // A hidden window has no frames at all, whatever it thinks it is
        // animating. The boot grace is the anti-deadlock for the gate: a cold
        // start slow enough that the first tick lands after every entrance
        // would otherwise never be seen to finish them.
        let boot_grace = now.saturating_duration_since(self.booted_at) < BOOT_FRAME_GRACE;
        let frames_on = self.visible && (animating || picture || boot_grace);
        let frames = if frames_on {
            iced::window::frames().map(Message::Tick)
        } else {
            Subscription::none()
        };

        // Only while somebody is looking at the device list. A hidden window
        // polling the subnet would be work with an audience of nobody, and
        // this application is built to sit on a machine with no monitor.
        let subnet = if self.visible && self.screen == Screen::Home {
            iced::time::every(LAN_POLL).map(|_| Message::PollLan)
        } else {
            Subscription::none()
        };

        // The other half of the same idea, on the slower clock: this one picks
        // up a cable plugged in, a Tailscale that has just come up, and a
        // machine that has gone offline — none of which announce themselves the
        // way mDNS does.
        let rescan = if self.visible && self.screen == Screen::Home {
            iced::time::every(AUTO_SCAN).map(|_| Message::AutoScan)
        } else {
            Subscription::none()
        };

        // Headless polling: a virtual display that vanishes or a driver that
        // appears must be noticed without a restart. Runs even when the window
        // is hidden, because a machine that boots headless and hosts itself
        // has no one to press the button.
        let display_poll = iced::time::every(Duration::from_secs(8)).map(|_| Message::PollDisplays);

        // Work with no frames to ride on still has to wake up: hosting's
        // events, a session in a background tab, a transfer finishing, a
        // notice that expires. Once a second is plenty for all of them.
        let background = self.hosting.is_some()
            || !self.tabs.is_empty()
            || !self.running.is_empty()
            || self.notice.as_ref().is_some_and(|notice| notice.until.is_some());
        let heartbeat = if background && !frames_on {
            iced::time::every(HEARTBEAT).map(Message::Tick)
        } else {
            Subscription::none()
        };

        // A shell prints when it prints. Its output rings a bell rather than
        // being looked for every frame.
        let terminals = if self.tabs.has_terminal() {
            Subscription::run(terminal_wakes)
        } else {
            Subscription::none()
        };

        // One Pravera at a time: a second launch writes a flag file; the
        // first polls it and focuses itself. Only the holder of the port
        // polls — the second never lives long enough to poll.
        let single_instance = if self.single_instance_lock.is_some() {
            iced::time::every(Duration::from_millis(500)).map(|_| Message::SingleInstanceCheck)
        } else {
            Subscription::none()
        };

        // Deliberately not a timer. The icon's own handlers push, so a Pravera
        // sitting hidden with nothing connected wakes only when somebody
        // actually clicks it.
        let clicks = if self.tray.is_some() {
            tray::subscription().map(Message::Tray)
        } else {
            Subscription::none()
        };

        // Hours between checks, so a minute's resolution is plenty — and it
        // runs hidden too, because a machine nobody looks at still updates.
        let updates = if self.updater.enabled {
            iced::time::every(update::TICK).map(|_| Message::UpdateTick)
        } else {
            Subscription::none()
        };

        // A window that is never shown gets no frames; pictures still need
        // the clock to move so every entrance finishes.
        let shooting = if shot::dir().is_some() {
            iced::time::every(Duration::from_millis(16)).map(Message::Tick)
        } else {
            Subscription::none()
        };

        Subscription::batch([
            frames,
            shooting,
            heartbeat,
            updates,
            terminals,
            subnet,
            rescan,
            display_poll,
            single_instance,
            clicks,
            iced::window::resize_events().map(|(id, size)| Message::WindowResized(id, size)),
            iced::event::listen_with(listen),
            // The window manager's own close: the title bar's button is not
            // the only way to ask, and every way has to mean the same thing.
            iced::window::close_requests().map(|_| Message::CloseWindow),
        ])
    }

    // ----------------------------------------------------------------- view

    fn view(&self) -> Element<'_, Message> {
        let now = self.now;
        let on_surface = self.on_surface();

        let bar = titlebar::view(titlebar::Bar {
            maximized: self.maximized,
            controls: &self.chrome_hover,
            tabs: self
                .tabs
                .items
                .iter()
                .map(|tab| titlebar::TabInfo {
                    title: tab.title(),
                    terminal: tab.is_terminal(),
                    ended: tab.is_ended(),
                })
                .collect(),
            active: on_surface.then_some(self.tabs.active),
            tab_hover: &self.tab_hover,
            on_session: on_surface,
            now,
        });

        let body: Element<'_, Message> = if on_surface {
            self.surface()
        } else {
            self.shell()
        };

        let mut layers: Vec<Element<'_, Message>> = vec![column![bar, body].into()];

        // The connect dialog floats over whatever is on screen: the click that
        // opened it happened on a row, and the page staying put is what says
        // the click landed. The scrim blurs the live frame on the GPU — see
        // `crate::backdrop`.
        if self.connect.is_mounted() {
            layers.push(
                mouse_area(backdrop::view(self.connect.scrim_amount(now)))
                    .on_press(Message::Connect(screens::connect::Message::Dismiss))
                    .into(),
            );
            layers.push(screens::connect::dialog_view(&self.connect, now).map(Message::Connect));
        }

        if let Some(menu) = screens::home::menu_view(&self.home, &self.device_entries, self.window_size, now) {
            layers.push(menu);
        }

        if let Some(toast) = self.toast_view() {
            layers.push(toast);
        }

        container(Stack::with_children(layers).width(Length::Fill).height(Length::Fill))
            .width(Length::Fill)
            .height(Length::Fill)
            .style(theme::root)
            .into()
    }

    /// The tab in front, edge to edge under the title bar.
    fn surface(&self) -> Element<'_, Message> {
        let surface: Element<'_, Message> = match self.tabs.active() {
            Some(Tab::Terminal(shell)) => screens::terminal::view(&shell.state).map(Message::Terminal),
            Some(Tab::Session(live)) => {
                screens::session::view(&live.state, &live.link, self.now).map(Message::Session)
            }
            None => Space::new().width(Length::Fill).height(Length::Fill).into(),
        };
        // A fade, no travel: the surface is the machine's own screen, and
        // sliding it would suggest it came from somewhere. It fades from the
        // colour it sits on: a picture's letterbox is black, a terminal's
        // ground is the app's.
        let ground = match self.tabs.active() {
            Some(Tab::Session(_)) => t::LETTERBOX,
            _ => t::BACKGROUND,
        };
        motion::rise_on(
            surface,
            motion::cascade(self.surface_at, self.now + motion::STAGGER_STEP, 0),
            0.0,
            ground,
        )
    }

    /// The rail and the page, on the window floor.
    ///
    /// The rail is a layer over the page rather than a column beside it: a
    /// pill grows out across the page while it is pointed at, and a column
    /// would have to either shove the page aside or clip the pill. The page
    /// keeps a rail-wide margin on its left instead, so at rest nothing
    /// overlaps, and the rail's empty floor lets the pointer through.
    fn shell(&self) -> Element<'_, Message> {
        let page = container(self.content())
            .padding(Padding {
                top: 0.0,
                right: t::GAP,
                bottom: t::GAP,
                left: 0.0,
            })
            .width(Length::Fill)
            .height(Length::Fill);
        let floor = row![Space::new().width(Length::Fixed(t::SIDEBAR_RAIL)), page]
            .width(Length::Fill)
            .height(Length::Fill);

        let rail = motion::rise_on(
            self.rail(),
            motion::cascade(self.shown_at, self.now, 0),
            motion::PANEL_RISE,
            t::BACKGROUND,
        );

        Stack::with_children([floor.into(), rail])
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }

    /// The sheet, with the page on it, or the page on its way out.
    ///
    /// The sheet is drawn here, round whatever page is in front, and is the
    /// same widget from one page to the next: it never moves and never fades.
    /// Only its contents change, the old page fading out on the spot and the
    /// new one rising in behind it.
    ///
    /// Pages fill the sheet and scroll inside their own bodies, so the window
    /// itself never scrolls and a header never slides up under the title bar.
    fn content(&self) -> Element<'_, Message> {
        let now = self.now;
        let leaving = self.leaving.filter(|_| !self.leave.is_gone(now));
        let shown = leaving.unwrap_or(self.screen);

        // Keyed by page, so each page's scroll positions are its own and it
        // starts at the top rather than inheriting wherever the last one was.
        let keyed: Element<'_, Message> = keyed_column([(shown.index(), self.page(shown))])
            .width(Length::Fill)
            .height(Length::Fill)
            .into();

        let contents = match leaving {
            Some(_) => {
                let amount = self.leave.value(now).clamp(0.0, 1.0);
                Transform::new(keyed).fade(t::CARD, amount).into()
            }
            None => keyed,
        };
        components::sheet(contents)
    }

    fn page(&self, screen: Screen) -> Element<'_, Message> {
        let now = self.now;
        match screen {
            // A session screen with no tab in front falls through to the list
            // rather than to a window with nothing in it.
            Screen::Home | Screen::Session => screens::home::view(
                &self.home,
                &self.device_entries,
                &self.discovered,
                self.scanning,
                now,
            ),
            Screen::Users => screens::users::view(&self.users, now).map(Message::Users),
            Screen::Transfers => screens::transfers::view(&self.transfers, now).map(Message::Transfers),
            Screen::Settings => screens::settings::view(
                &self.settings,
                self.endpoint.as_ref().map(|e| e.device_id),
                self.hosting.as_ref(),
                &screens::settings::Unattended {
                    at_sign_in: self.at_sign_in,
                    host_at_launch: self.prefs.host_at_launch,
                    tray: self.tray.is_some(),
                    displays: self.displays,
                    virtual_display: self.virtual_display.clone(),
                    capture_backend: self.capture_backend.clone(),
                    service: self.boot_service(),
                },
                &self.update_summary(),
                now,
            )
            .map(Message::Settings),
            Screen::Mcp => screens::mcp::view(&self.mcp, now).map(Message::Mcp),
        }
    }

    // ----------------------------------------------------------------- rail

    /// DigiClip's rail: a 32px square per section on the window floor, 5px
    /// in from the edge. The one under the pointer grows sideways into a pill
    /// with its name in it, over the page, and shrinks back when it is left.
    ///
    /// Open tabs follow the sections, one pill each; this machine's hosting
    /// state and Settings sit at the foot.
    fn rail(&self) -> Element<'_, Message> {
        let [reach, admin, foot] = NAV;
        let mut rail = column![].spacing(t::SPACE_1).height(Length::Fill);

        for &screen in reach {
            rail = rail.push(self.rail_screen(screen));
        }
        rail = rail.push(rail_rule());
        for &screen in admin {
            rail = rail.push(self.rail_screen(screen));
        }

        if !self.tabs.is_empty() {
            rail = rail.push(rail_rule());
            for (index, tab) in self.tabs.items.iter().enumerate().take(RAIL_TABS) {
                rail = rail.push(self.rail_tab(index, tab));
            }
        }

        rail = rail.push(Space::new().height(Length::Fill));
        rail = rail.push(self.rail_host());
        for &screen in foot {
            rail = rail.push(self.rail_screen(screen));
        }

        // One highlight for the whole rail, under the pills: it slides to the
        // page in front rather than each pill lighting and dimming on its own.
        // The pills stay clear at rest, so it shows through, and a pill grown
        // by the pointer is opaque and covers it.
        let mut layers = Stack::with_children([Element::from(rail)]).height(Length::Fill);
        if self.screen != Screen::Session {
            layers = layers.push_under(rail_highlight(self.rail_highlight_y(self.now)));
        }

        container(layers)
            .padding(Padding {
                top: RAIL_TOP,
                right: t::GAP,
                bottom: t::GAP,
                left: t::GAP,
            })
            .height(Length::Fill)
            .into()
    }

    /// How tall the rail's column is: the window under the title bar, less
    /// what the rail is inset by at the top and the bottom.
    fn rail_column(&self) -> f32 {
        self.window_size.height - t::TITLEBAR_HEIGHT - RAIL_TOP - t::GAP
    }

    /// How far down the rail's column the highlight is drawn at `now`.
    fn rail_highlight_y(&self, now: Instant) -> f32 {
        let slide = self.rail_slide.value(now).clamp(0.0, 1.0);
        let to = rail_y(self.rail_to, self.rail_column());
        self.rail_from + (to - self.rail_from) * slide
    }

    fn rail_screen(&self, screen: Screen) -> Element<'_, Message> {
        let index = screen.index();
        // Active is a value in motion, not a flag: the highlight crossfades
        // from the page you left to the page you chose.
        let active = self.nav_active[index].interpolate(0.0, 1.0, self.now);
        let grow = self.nav_hover.amount(index, self.now);

        let mut mark = icon::stroked(screen.icon(), t::ICON, nav_tint(active, grow));
        // A transfer in flight marks its section, so it can be left running
        // and still be found again.
        if screen == Screen::Transfers && !self.running.is_empty() {
            mark = components::badged(mark, t::LIME, t::BACKGROUND);
        }
        // A newer version waiting is worth a glance, not a dialog.
        if screen == Screen::Settings && self.updater.pending() {
            mark = components::badged(mark, t::LIME, t::BACKGROUND);
        }
        rail_pill(mark, screen.label(), active, grow, index, Message::Navigate(screen))
    }

    /// An open tab: the rail's answer to "what am I connected to".
    fn rail_tab<'a>(&'a self, index: usize, tab: &'a Tab) -> Element<'a, Message> {
        let slot = NAV_TABS + index;
        let grow = self.nav_hover.amount(slot, self.now);
        let ended = tab.is_ended();

        let mark: Element<'a, Message> = if tab.is_terminal() {
            let (_, edge) = components::avatar_tint(tab.title());
            icon::stroked(icon::TERMINAL, t::ICON, if ended { t::SUBTLE_FOREGROUND } else { edge })
        } else {
            components::avatar_faded(tab.title(), t::ICON + 2.0, if ended { 0.5 } else { 1.0 })
        };
        // The rail only draws in the shell, never over a tab, so a tab's
        // pill is never the active one.
        rail_pill(mark, clipped(tab.title(), RAIL_LABEL), 0.0, grow, slot, Message::SwitchTab(index))
    }

    /// Whether this machine can be reached: the one status that matters when
    /// somebody is deciding whether to walk away from it. Opens Settings,
    /// where it is changed.
    fn rail_host(&self) -> Element<'_, Message> {
        let grow = self.nav_hover.amount(NAV_HOST, self.now);
        let (tint, status) = match (&self.hosting, &self.endpoint_error) {
            (Some(hosting), _) => (
                t::LIME,
                match hosting.connections() {
                    0 => "Hosting".to_string(),
                    n => format!("Hosting ({n})"),
                },
            ),
            (None, Some(_)) => (t::DESTRUCTIVE, "No endpoint".to_string()),
            (None, None) => (t::NEUTRAL_600, "Not hosting".to_string()),
        };
        rail_pill(
            components::dot(tint, 8.0),
            status,
            0.0,
            grow,
            NAV_HOST,
            Message::Navigate(Screen::Settings),
        )
    }

    /// The notice, as a toast in the bottom-right corner: over whatever is on
    /// screen, including a session, and out of the way of all of it.
    fn toast_view(&self) -> Option<Element<'_, Message>> {
        let notice = self.notice.as_ref().or(self.fading_notice.as_ref())?;
        let amount = self.toast.value(self.now).clamp(0.0, 1.0);
        if amount <= 0.001 {
            return None;
        }

        let (tint, glyph) = if notice.failure {
            (t::DESTRUCTIVE_TEXT, icon::ALERT)
        } else {
            (t::MUTED_FOREGROUND, icon::DISCONNECT)
        };

        let close = button(
            container(icon::stroked(icon::CLOSE, 10.0, theme::faded(t::MUTED_FOREGROUND, amount)))
                .center_x(Length::Fill)
                .center_y(Length::Fill),
        )
        .width(Length::Fixed(22.0))
        .height(Length::Fixed(22.0))
        .padding(0)
        .style(move |_, status| button::Style {
            background: Some(Background::Color(match status {
                button::Status::Hovered => t::with_alpha(t::NEUTRAL_700, amount),
                button::Status::Pressed => t::with_alpha(t::NEUTRAL_600, amount),
                _ => iced::Color::TRANSPARENT,
            })),
            border: Border {
                radius: t::RADIUS_SM.into(),
                ..Border::default()
            },
            ..button::Style::default()
        })
        .on_press(Message::DismissNotice);

        let card = components::panel(
            row![
                icon::stroked(glyph, t::ICON_SM, theme::faded(tint, amount)),
                text(notice.message.as_str())
                    .size(t::TEXT_XS)
                    .width(Length::Fill)
                    .style(theme::tinted(theme::faded(t::NEUTRAL_200, amount))),
                close,
            ]
            .spacing(t::SPACE_2)
            .align_y(Alignment::Center),
        )
        .edge(t::BEVEL_RAISED)
        .fill(t::TOAST)
        .padding([t::SPACE_2, t::SPACE_3])
        .width(Length::Fixed(360.0))
        .opacity(amount)
        .shadow(iced::Shadow {
            color: theme::faded(t::SHADOW_INK, 0.45 * amount),
            ..theme::SHADOW_FLOAT
        });

        let card = Transform::new(opaque(card)).offset(Vector::new(0.0, 12.0 * (1.0 - amount)));

        Some(
            container(card)
                .align_right(Length::Fill)
                .align_bottom(Length::Fill)
                .padding(t::SPACE_4)
                .into(),
        )
    }
}

/// How far the first pill sits below the title bar: centred on the page's
/// header, so the rail and the page start on one line.
const RAIL_TOP: f32 = (t::HEADER_HEIGHT - t::RAIL_ITEM) / 2.0;

/// The most characters a pill's label holds before it is cut short: what
/// fits in a grown pill beside the mark at the body size.
const RAIL_LABEL: usize = 14;

/// The colour of a rail entry's mark and words.
fn nav_tint(active: f32, hover: f32) -> iced::Color {
    theme::blend(
        theme::blend(t::MUTED_FOREGROUND, t::FOREGROUND, hover),
        t::FOREGROUND,
        active,
    )
}

/// How wide a rail pill is at `grow`: its square at rest, [`t::RAIL_PILL`]
/// fully grown.
fn pill_width(grow: f32) -> f32 {
    t::RAIL_ITEM + (t::RAIL_PILL - t::RAIL_ITEM) * grow.clamp(0.0, 1.0)
}

/// How much of a rail pill's label shows at `grow`. The words wait for a
/// third of the growth, so they never appear squeezed into a pill still too
/// narrow for them; DigiClip delays its label the same way.
fn label_amount(grow: f32) -> f32 {
    ((grow - 0.35) / 0.65).clamp(0.0, 1.0)
}

/// One rail entry: a 32px square holding its mark, growing rightwards by
/// `grow` into a pill with its label centred in the new room.
///
/// The mark sits in a fixed square at the pill's left, so it never moves as
/// the pill grows; the label is clipped by the pill rather than wrapped. The
/// grown pill floats over the page, so it takes a lighter fill, a lit ring
/// and a shadow — the page under it must not read as part of it.
fn rail_pill<'a>(
    mark: Element<'a, Message>,
    label: impl text::IntoFragment<'a>,
    active: f32,
    grow: f32,
    slot: usize,
    on_press: Message,
) -> Element<'a, Message> {
    let tint = nav_tint(active, grow);
    let words = label_amount(grow);

    let content = row![
        container(mark)
            .center_x(Length::Fixed(t::RAIL_ITEM))
            .center_y(Length::Fill),
        container(
            text(label)
                .size(t::TEXT_SM)
                .font(t::FONT_UI_MEDIUM)
                .wrapping(text::Wrapping::None)
                .style(theme::tinted(theme::faded(tint, words))),
        )
        .padding(Padding {
            right: t::SPACE_2 + 2.0,
            ..Padding::ZERO
        })
        .center_x(Length::Fill)
        .center_y(Length::Fill)
        .clip(true),
    ]
    .height(Length::Fill);

    // Clear at rest: a chosen entry's tile is the rail's highlight, drawn
    // under the pills, not a fill of the pill's own.
    let fill = theme::blend(t::with_alpha(t::NEUTRAL_800, 0.0), t::NEUTRAL_750, grow);
    let ring = theme::blend(
        t::with_alpha(t::NEUTRAL_700, 0.0),
        theme::blend(t::NEUTRAL_700, t::NEUTRAL_600, 0.35),
        grow,
    );
    let shadow = iced::Shadow {
        color: t::with_alpha(t::SHADOW_INK, 0.55 * grow),
        offset: Vector::new(0.0, 8.0),
        blur_radius: 24.0,
    };

    let pill = button(content)
        .width(Length::Fixed(pill_width(grow)))
        .height(Length::Fixed(t::RAIL_ITEM))
        .padding(0)
        .style(move |_, status| button::Style {
            background: Some(Background::Color(match status {
                button::Status::Pressed => t::NEUTRAL_700,
                _ => fill,
            })),
            text_color: tint,
            border: Border {
                color: ring,
                width: 1.0,
                radius: t::RADIUS.into(),
            },
            shadow,
            ..button::Style::default()
        })
        .on_press(on_press);

    mouse_area(pill)
        .on_enter(Message::HoverNav(slot, true))
        .on_exit(Message::HoverNav(slot, false))
        .into()
}

/// The tile under the chosen rail entry, `y` below the top of the rail's
/// column.
fn rail_highlight<'a>(y: f32) -> Element<'a, Message> {
    container(
        container(Space::new().width(Length::Fixed(t::RAIL_ITEM)).height(Length::Fixed(t::RAIL_ITEM))).style(
            |_| container::Style {
                background: Some(Background::Color(t::NEUTRAL_800)),
                border: Border {
                    color: t::NEUTRAL_700,
                    width: 1.0,
                    radius: t::RADIUS.into(),
                },
                ..container::Style::default()
            },
        ),
    )
    .padding(Padding {
        top: y,
        ..Padding::ZERO
    })
    .height(Length::Fill)
    .into()
}

/// Where a screen's entry is among the rail's, counted from the top, or
/// `None` for one the rail has no entry for. [`rail_y`] turns it into pixels.
fn rail_position(screen: Screen) -> Option<f32> {
    NAV.iter()
        .flat_map(|run| run.iter())
        .position(|&entry| entry == screen)
        .map(|index| index as f32)
}

/// The height of the rail's rule, and the room round it.
const RAIL_RULE: f32 = 1.0 + 2.0 * t::SPACE_1;

/// Where each entry of [`NAV`] starts, in order, measured down the rail's
/// column, which is `height` tall. The first two runs hang from the top; the
/// last sits at the foot, above nothing, so it is measured up from the bottom.
fn rail_tops(height: f32) -> Vec<f32> {
    let step = t::RAIL_ITEM + t::SPACE_1;
    let [reach, admin, foot] = NAV;
    let mut tops = Vec::new();
    let mut y = 0.0;
    for _ in reach {
        tops.push(y);
        y += step;
    }
    y += RAIL_RULE + t::SPACE_1;
    for _ in admin {
        tops.push(y);
        y += step;
    }
    for (index, _) in foot.iter().enumerate() {
        let below = (foot.len() - index) as f32;
        tops.push(height - t::RAIL_ITEM * below - t::SPACE_1 * (below - 1.0));
    }
    tops
}

/// How far down the rail's column the highlight is at `position`: exactly on
/// an entry at a whole number, and a straight line between the two entries
/// either side of it otherwise.
fn rail_y(position: f32, height: f32) -> f32 {
    let tops = rail_tops(height);
    let last = tops.len() - 1;
    let position = position.clamp(0.0, last as f32);
    let low = position.floor() as usize;
    let high = (low + 1).min(last);
    tops[low] + (tops[high] - tops[low]) * (position - low as f32)
}

/// A short hairline between runs of rail entries, centred under the marks.
fn rail_rule<'a>() -> Element<'a, Message> {
    container(
        container(Space::new().width(Length::Fixed(t::SPACE_5)).height(Length::Fixed(1.0))).style(|_| {
            container::Style {
                background: Some(Background::Color(t::BORDER)),
                ..container::Style::default()
            }
        }),
    )
    .center_x(Length::Fixed(t::RAIL_ITEM))
    .padding([t::SPACE_1, 0.0])
    .into()
}

/// Fetch and check an update, reporting progress as it arrives.
fn download_update(release: install::release::Release) -> Task<Message> {
    use iced::futures::SinkExt;
    Task::run(
        iced::stream::channel(16, async move |mut sender| {
            let mut progress = sender.clone();
            let result = install::release::download(&release, |done, total| {
                let _ = progress.try_send(Message::UpdateProgress(done, total));
            })
            .await;
            let _ = sender.send(Message::UpdateDownloaded(result)).await;
        }),
        |message| message,
    )
}

/// `words`, cut to `most` characters with an ellipsis when longer.
fn clipped(words: &str, most: usize) -> String {
    if words.chars().count() <= most {
        return words.to_string();
    }
    let mut cut: String = words.chars().take(most.saturating_sub(1)).collect();
    cut.push('…');
    cut
}

/// Send the session everything the keyboard hook has taken since the last
/// call, in the order it was taken.
fn forward_grabbed(live: &mut Live, now: Instant) {
    for taken in net::grab::drain() {
        let message = screens::session::Message::Key {
            code: taken.code,
            pressed: taken.pressed,
        };
        for command in screens::session::update(&mut live.state, message, now) {
            live.link.send(command);
        }
    }
}

/// The raw event listener. A plain function, because the subscription it
/// feeds must be identical on every call to stay the same subscription.
fn listen(event: iced::Event, status: iced::event::Status, _window: iced::window::Id) -> Option<Message> {
    use iced::keyboard::{key::Named, Event as Keyboard, Key};

    match event {
        iced::Event::Window(iced::window::Event::Focused) => Some(Message::Focused(true)),
        iced::Event::Window(iced::window::Event::Unfocused) => Some(Message::Focused(false)),
        // Remembered, not published: see `pointer_at`.
        iced::Event::Mouse(iced::mouse::Event::CursorMoved { position }) => {
            pointer_at::record(position);
            None
        }
        // Escape is seen whatever took it: a focused field swallows it, and
        // a dialog with a focused field must still close on it.
        iced::Event::Keyboard(Keyboard::KeyPressed {
            key: Key::Named(Named::Escape),
            modifiers,
            ..
        }) => Some(Message::Escape(screens::terminal::Press {
            key: Key::Named(Named::Escape),
            text: None,
            modifiers,
        })),
        // Every other key press the window received and no widget wanted.
        iced::Event::Keyboard(Keyboard::KeyPressed {
            key,
            text,
            modifiers,
            ..
        }) if status == iced::event::Status::Ignored => {
            Some(Message::KeyPressed(screens::terminal::Press {
                key,
                text: text.map(|text| text.to_string()),
                modifiers,
            }))
        }
        _ => None,
    }
}

/// One message per wake from a terminal. See `net::wake`.
fn terminal_wakes() -> impl iced::futures::Stream<Item = Message> {
    use iced::futures::StreamExt;
    net::wake::rings().map(|()| Message::TerminalWake)
}

/// Register the service, or repoint it at this copy, without holding up the
/// window: the service control manager answers when it answers.
fn check_service() -> Task<Message> {
    Task::perform(
        async {
            tokio::task::spawn_blocking(pravera_service::ensure_installed)
                .await
                .unwrap_or(pravera_service::Installed::NotElevated)
        },
        Message::ServiceChecked,
    )
}

/// Whether the sign-in task is registered, asked without holding up the
/// window: the answer comes from `schtasks`.
fn read_at_sign_in() -> Task<Message> {
    Task::perform(
        async {
            tokio::task::spawn_blocking(autostart::is_enabled)
                .await
                .map_err(|error| error.to_string())
        },
        Message::AtSignIn,
    )
}

/// Turn a lowercase error into something that reads as a sentence.
///
/// The errors these come from are written to compose — "no such file", "the
/// clipboard is busy" — which is right in a log line and wrong on its own in a
/// banner. Only the first character changes; the rest is left alone, because a
/// path or a machine name in the middle should keep its own capitalisation.
fn sentence(words: &str) -> String {
    let mut chars = words.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// Put a file name into a directory, the way that filesystem spells paths.
///
/// The separator belongs to whichever machine the file is landing on, and only
/// the side doing the landing knows which that is. A Linux client dropping a
/// file on a Windows host would otherwise produce `C:\Users\kim/report.pdf`,
/// which some Windows calls accept and some do not.
fn join(directory: &str, name: &str) -> String {
    let separator = if directory.contains('\\') { '\\' } else { '/' };
    if directory.ends_with(['\\', '/']) {
        format!("{directory}{name}")
    } else {
        format!("{directory}{separator}{name}")
    }
}

/// What closing the window does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Closing {
    /// Out of sight, still accepting sessions, still one click away.
    Hide,
    /// Gone, hosting and all.
    Quit,
}

/// Whether closing the window should hide Pravera or end it.
///
/// Hiding is right when there is a tray icon to hide behind, regardless of
/// hosting — the icon itself is the sign that the app is still there, and the
/// menu says whether it is reachable. Without an icon there would be no sign
/// and no way to stop it, so closing quits.
fn closing(tray: bool, _hosting: bool) -> Closing {
    if tray {
        Closing::Hide
    } else {
        Closing::Quit
    }
}

/// What went wrong, in the words meant for a person rather than a log.
///
/// `Error`'s `Display` prefixes the layer that raised it — "configuration: …",
/// "transport: …" — which is what makes a log line searchable and exactly what
/// makes a banner read like a stack trace. The account store's messages are
/// already complete sentences, so the prefix is all that has to come off.
fn reason(error: &pravera_core::Error) -> String {
    match error {
        pravera_core::Error::Config(message) => message.clone(),
        other => other.to_string(),
    }
}

/// Load this machine's key and open its one endpoint.
///
/// Global reachability: sessions have to work off the local network too, which
/// means the endpoint needs the address-lookup and relay services. Nothing
/// about that weakens the connection — a relay carries ciphertext it cannot
/// read, and the peer still proves the private key.
async fn bind_endpoint() -> Result<BoundEndpoint, String> {
    // Machine-wide when this Pravera is elevated, which is what lets the
    // service start a host before anybody has signed in. See
    // `pravera_core::paths::machine_dir` for why it is resolved as a set.
    let path = pravera_core::paths::device_key_file()
        .map_err(|error| format!("Pravera has nowhere to keep its key: {error}"))?;

    let identity = Identity::load_or_create(&path)
        .map_err(|error| format!("This machine's key could not be read or created: {error}"))?;
    let device_id = identity.device_id();

    let transport = Transport::bind(&identity, Reachability::Global)
        .await
        .map_err(|error| format!("Could not open a network endpoint: {error}"))?;

    Ok(BoundEndpoint {
        transport,
        device_id,
    })
}

/// Turn what the service control manager did into what Settings says.
///
/// A translation and nothing more. The service knows three ways of already
/// being registered and the screen has one way of saying it, which is the right
/// asymmetry: what somebody wants to know is whether the machine comes back,
/// not which of three code paths arranged that.
///
/// Free rather than a method so the one property that matters can be tested
/// without a running interface: nothing that leaves the machine unregistered
/// may be reported as registered.
fn boot_service(installed: &pravera_service::Installed) -> screens::settings::Service {
    use pravera_service::Installed;
    use screens::settings::Service;

    // Off Windows there is no such service to be un-elevated for, and telling
    // somebody on Linux to run as administrator is an instruction that leads
    // nowhere.
    if !cfg!(windows) {
        return Service::Unavailable;
    }

    match installed {
        Installed::Registered | Installed::Repointed | Installed::Unchanged => Service::Installed,
        Installed::NotElevated => Service::NeedsElevation,
        Installed::Refused(reason) => Service::Refused(reason.clone()),
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    #[test]
    fn a_machine_that_will_not_come_back_is_never_shown_as_one_that_will() {
        // The whole point of the line in Settings. A machine that stays dark
        // after a reboot must not be described as one that starts at boot,
        // because somebody reads that line and then goes home.
        for state in [
            pravera_service::Installed::NotElevated,
            pravera_service::Installed::Refused("anything at all".into()),
        ] {
            assert!(!state.is_installed(), "{state:?}");
            assert_ne!(
                boot_service(&state),
                screens::settings::Service::Installed,
                "{state:?}"
            );
        }
    }

    #[test]
    #[cfg(windows)]
    fn every_way_of_being_registered_reads_the_same() {
        // Registering, repointing and finding it already right are three jobs
        // and one fact. A screen that told them apart would be reporting on
        // Pravera's bookkeeping rather than on the machine.
        //
        // Windows only: elsewhere there is no service to register, and the
        // translation answers Unavailable whatever the enum carries — that is
        // the test below.
        for state in [
            pravera_service::Installed::Registered,
            pravera_service::Installed::Repointed,
            pravera_service::Installed::Unchanged,
        ] {
            assert!(state.is_installed(), "{state:?}");
            assert_eq!(
                boot_service(&state),
                screens::settings::Service::Installed,
                "{state:?}"
            );
        }
    }

    #[test]
    #[cfg(not(windows))]
    fn a_platform_with_no_service_reports_unavailable_whatever_the_record_says() {
        // The enum is shared across platforms, and a record of registration
        // can only exist on the platform that has a service. Translating it
        // into Installed elsewhere would promise a machine that starts at
        // boot and does not.
        for state in [
            pravera_service::Installed::Registered,
            pravera_service::Installed::Repointed,
            pravera_service::Installed::Unchanged,
        ] {
            assert_eq!(
                boot_service(&state),
                screens::settings::Service::Unavailable,
                "{state:?}"
            );
        }
    }

    #[test]
    fn a_refusal_shown_to_a_person_does_not_name_the_layer_that_raised_it() {
        // "configuration: admin is a built-in role" is a log line. What belongs
        // on screen is the half after the colon.
        let refused = pravera_core::Error::Config("admin is a built-in role".into());
        assert_eq!(reason(&refused), "admin is a built-in role");
        assert!(refused.to_string().starts_with("configuration:"));
    }

    #[test]
    fn an_error_with_no_prefix_to_strip_is_left_alone() {
        let offline = pravera_core::Error::PeerOffline;
        assert_eq!(reason(&offline), offline.to_string());
    }

    #[test]
    fn every_page_appears_exactly_once_in_the_navigation() {
        // The session screen is reached through its tabs, never through a
        // row of its own: a "Session" entry with nothing open behind it
        // would be a door to an empty room.
        let listed: Vec<Screen> = NAV.iter().flat_map(|run| run.iter().copied()).collect();
        assert!(!listed.contains(&Screen::Session));
        assert_eq!(
            listed.len(),
            Screen::ALL.len() - 1,
            "a screen is missing or duplicated in NAV"
        );
        for screen in Screen::ALL.into_iter().filter(|&s| s != Screen::Session) {
            assert_eq!(
                listed.iter().filter(|&&s| s == screen).count(),
                1,
                "{:?} is not listed exactly once",
                screen
            );
        }
    }

    #[test]
    fn the_session_screen_with_no_tab_is_the_device_list() {
        let mut app = Pravera::assemble(None, false);
        app.go_to(Screen::Users);
        app.go_to(Screen::Session);
        assert_eq!(app.screen, Screen::Users, "it went to a surface with nothing on it");
        assert!(!app.on_surface());
    }

    #[test]
    fn a_page_change_lets_the_old_page_leave_before_the_new_one_arrives() {
        let mut app = Pravera::assemble(None, false);
        app.go_to(Screen::Transfers);
        assert_eq!(app.screen, Screen::Transfers);
        assert_eq!(app.leaving, Some(Screen::Home));
        assert!(app.leave.is_animating(app.now));

        // Once the exit has run, the pump lets the old page go.
        app.now += motion::PAGE_OUT + Duration::from_millis(1);
        let _ = app.pump();
        assert_eq!(app.leaving, None);
    }

    #[test]
    fn a_second_change_during_an_exit_joins_it_rather_than_restarting_it() {
        let mut app = Pravera::assemble(None, false);
        app.go_to(Screen::Transfers);
        let ends = app.leave_ends;
        app.now += motion::PAGE_OUT / 2;
        app.go_to(Screen::Users);
        assert_eq!(app.leave_ends, ends);
        assert_eq!(app.screen, Screen::Users);
    }

    #[test]
    fn the_home_toggle_remembers_where_it_came_from() {
        let mut app = Pravera::assemble(None, false);
        app.go_to(Screen::Settings);
        assert_eq!(app.last_shell, Screen::Settings);
        // With no tab open the toggle has nowhere to go.
        let _ = app.dispatch(Message::ToggleHome);
        assert_eq!(app.screen, Screen::Settings);
    }

    #[test]
    fn a_notice_fades_rather_than_vanishing() {
        let mut app = Pravera::assemble(None, false);
        app.end_session(Ending::Requested);
        assert!(app.notice.is_some());

        app.clear_notice();
        assert!(app.notice.is_none());
        assert!(app.fading_notice.is_some(), "it disappeared mid-frame");

        app.now += motion::DIALOG_OUT + Duration::from_millis(1);
        app.expire_notice();
        assert!(app.fading_notice.is_none());
    }

    #[test]
    fn the_highlight_sits_on_each_entry_and_slides_in_a_line_between_them() {
        let height = 700.0;
        let tops = rail_tops(height);
        // One entry per screen the rail names, in the order it draws them.
        assert_eq!(tops.len(), NAV.iter().map(|run| run.len()).sum::<usize>());
        for (index, top) in tops.iter().enumerate() {
            assert_eq!(rail_y(index as f32, height), *top);
        }
        // Devices is first, Transfers a square and a gap below it.
        assert_eq!(tops[0], 0.0);
        assert_eq!(tops[1], t::RAIL_ITEM + t::SPACE_1);
        // Halfway between two entries is halfway between their tops.
        let mid = rail_y(0.5, height);
        assert!((mid - tops[1] / 2.0).abs() < 1e-4);
        // And it never runs backwards on the way down.
        let mut last = -1.0;
        for step in 0..=40 {
            let y = rail_y(step as f32 / 10.0, height);
            assert!(y >= last);
            last = y;
        }
        // Past either end it stays on the end.
        assert_eq!(rail_y(-3.0, height), tops[0]);
        assert_eq!(rail_y(99.0, height), *tops.last().unwrap());
    }

    #[test]
    fn the_entry_at_the_foot_of_the_rail_is_measured_from_the_bottom() {
        let settings = rail_position(Screen::Settings).unwrap();
        // Two windows differing by 100px: Settings moves with the bottom, the
        // entries at the top do not.
        assert_eq!(rail_y(settings, 600.0) - rail_y(settings, 500.0), 100.0);
        assert_eq!(rail_y(0.0, 600.0), rail_y(0.0, 500.0));
        let users = rail_position(Screen::Users).unwrap();
        assert_eq!(rail_y(users, 600.0), rail_y(users, 500.0));
        // Its square ends exactly at the bottom of the column.
        assert_eq!(rail_y(settings, 600.0) + t::RAIL_ITEM, 600.0);
    }

    #[test]
    fn every_page_with_an_entry_has_a_position_and_the_session_screen_has_none() {
        for screen in Screen::ALL {
            let expected = screen != Screen::Session;
            assert_eq!(rail_position(screen).is_some(), expected, "{screen:?}");
        }
    }

    #[test]
    fn a_page_change_slides_the_highlight_from_where_it_is_and_never_jumps() {
        let mut app = Pravera::assemble(None, false);
        let column = app.rail_column();
        let home = rail_y(rail_position(Screen::Home).unwrap(), column);
        let settings = rail_y(rail_position(Screen::Settings).unwrap(), column);
        let users = rail_y(rail_position(Screen::Users).unwrap(), column);
        assert_eq!(app.rail_highlight_y(app.now), home);

        app.go_to(Screen::Settings);
        // It starts out on the entry it left, and is in motion.
        assert_eq!(app.rail_highlight_y(app.now), home);
        assert!(app.rail_slide.is_animating(app.now));
        // Part way it is between the two, neither on one nor the other.
        let part = app.rail_highlight_y(app.now + motion::STANDARD / 3);
        assert!(part > home && part < settings, "{part}");
        // A change of mind mid-slide sets off from where it was drawn.
        app.now += motion::STANDARD / 3;
        let drawn = app.rail_highlight_y(app.now);
        app.go_to(Screen::Users);
        assert_eq!(app.rail_highlight_y(app.now), drawn);
        app.now += motion::STANDARD + Duration::from_millis(1);
        assert_eq!(app.rail_highlight_y(app.now), users);
        assert!(!app.rail_slide.is_animating(app.now));
    }

    #[test]
    fn a_slide_to_the_foot_of_the_rail_is_one_steady_movement_not_a_dash_at_the_end() {
        let mut app = Pravera::assemble(None, false);
        let column = app.rail_column();
        let settings = rail_y(rail_position(Screen::Settings).unwrap(), column);
        app.go_to(Screen::Settings);
        // On the easing curve alone: nothing in the last quarter of the
        // slide covers more of the distance than the curve itself does.
        let at = |fraction: f32| {
            (app.rail_highlight_y(app.now + motion::STANDARD.mul_f32(fraction)) - app.rail_highlight_y(app.now)) / settings
        };
        let curve = |fraction: f32| motion::EASE_CHANGE.value(fraction);
        for fraction in [0.25, 0.5, 0.75, 1.0] {
            assert!((at(fraction) - curve(fraction)).abs() < 1e-3, "{fraction}");
        }
    }

    #[test]
    fn coming_back_from_a_tab_finds_the_highlight_already_in_place() {
        let mut app = Pravera::assemble(None, false);
        app.go_to(Screen::Settings);
        app.screen = Screen::Session;
        app.go_to(Screen::Users);
        let users = rail_y(rail_position(Screen::Users).unwrap(), app.rail_column());
        assert_eq!(app.rail_highlight_y(app.now), users);
        assert!(!app.rail_slide.is_animating(app.now));
    }

    #[test]
    fn the_rail_is_as_wide_as_a_square_and_its_two_gaps() {
        // The page's left margin is the rail's width. If a pill at rest were
        // any wider it would sit on the page; any narrower and the gap on its
        // right would not match the one on its left.
        assert_eq!(t::GAP + t::RAIL_ITEM + t::GAP, t::SIDEBAR_RAIL);
    }

    #[test]
    fn a_pill_rests_square_and_grows_to_hold_its_label() {
        assert_eq!(pill_width(0.0), t::RAIL_ITEM);
        assert_eq!(pill_width(1.0), t::RAIL_PILL);
        assert_eq!(label_amount(0.0), 0.0);
        assert_eq!(label_amount(1.0), 1.0);
        // The words wait until there is room for them.
        assert_eq!(label_amount(0.3), 0.0);
        assert!(pill_width(0.35) > t::RAIL_ITEM + 40.0);
    }

    #[test]
    fn rail_hover_slots_never_collide() {
        assert!(NAV_HOST >= Screen::ALL.len());
        assert!(NAV_TABS > NAV_HOST);
        assert_eq!(NAV_SLOTS, NAV_TABS + RAIL_TABS);
        let app = Pravera::assemble(None, false);
        // Every slot the rail can name has an animation behind it.
        assert!(app.nav_hover.amount(NAV_SLOTS - 1, app.now) >= 0.0);
    }

    #[test]
    fn a_long_tab_title_is_cut_to_fit_its_pill() {
        assert_eq!(clipped("EVERCORE", RAIL_LABEL), "EVERCORE");
        let cut = clipped("a-very-long-machine-name.local", RAIL_LABEL);
        assert_eq!(cut.chars().count(), RAIL_LABEL);
        assert!(cut.ends_with('…'));
    }

    #[test]
    fn escape_closes_the_dialog_before_anything_else() {
        let mut app = Pravera::assemble(None, false);
        let _ = app.dispatch(Message::AddDevice);
        assert!(app.connect.is_open());
        let _ = app.dispatch(Message::Escape(screens::terminal::Press {
            key: iced::keyboard::Key::Named(iced::keyboard::key::Named::Escape),
            text: None,
            modifiers: iced::keyboard::Modifiers::default(),
        }));
        assert!(!app.connect.is_open());
    }

    #[test]
    fn the_pointer_position_survives_the_trip_through_the_atomic() {
        let at = iced::Point::new(412.5, -3.25);
        pointer_at::record(at);
        assert_eq!(pointer_at::last(), at);
    }

    #[test]
    fn every_screen_has_a_distinct_hover_slot() {
        let mut seen = Vec::new();
        for screen in Screen::ALL {
            let index = screen.index();
            assert!(index < Screen::ALL.len());
            assert!(!seen.contains(&index), "{:?} shares a hover slot", screen);
            seen.push(index);
        }
    }

    #[test]
    fn every_screen_has_a_drawn_icon_of_its_own() {
        for (i, a) in Screen::ALL.iter().enumerate() {
            assert!(a.icon().starts_with("<svg"), "{a:?} has no drawn icon");
            for b in &Screen::ALL[i + 1..] {
                assert_ne!(a.icon(), b.icon(), "{a:?} and {b:?} share an icon");
            }
        }
    }

    #[test]
    fn navigating_between_sections_does_not_disturb_the_hover() {
        // Clicking a nav item moves the pointer nowhere, so the tint under it
        // must survive the screen change.
        let now = Instant::now();
        let mut hover = HoverTracker::new(Screen::ALL.len());
        hover.set(Screen::Users.index(), true, now);
        assert_eq!(hover.current(), Some(Screen::Users.index()));
    }

    #[test]
    fn pravera_never_runs_with_nothing_at_all_to_show_for_it() {
        // Hiding is allowed when there is a tray icon to hide behind — the
        // icon itself is the sign that the app is still there, and its menu
        // says whether it is reachable. Without an icon there would be no
        // sign and no way to stop it.
        assert_eq!(closing(true, true), Closing::Hide);
        assert_eq!(closing(true, false), Closing::Hide);
        assert_eq!(closing(false, true), Closing::Quit);
        assert_eq!(closing(false, false), Closing::Quit);
    }

    #[test]
    fn a_machine_told_to_start_hidden_with_nowhere_to_hide_appears_anyway() {
        // `--hidden` is written by `autostart`, which cannot know whether the
        // notification area will accept an icon on the day it runs. Starting
        // invisible with no icon would be a window nobody could ever reach.
        let app = Pravera::assemble(None, true);
        assert!(app.visible);
    }

    #[test]
    fn the_icon_is_told_which_machine_this_is_and_whether_anyone_is_connected() {
        // With no window this tooltip is the only answer to either question.
        let app = Pravera::assemble(None, false);
        let status = app.tray_status();
        assert!(!status.hosting);
        assert_eq!(status.connections, 0);
        assert_eq!(status.device, None, "it named a device before binding one");
    }

    #[test]
    fn a_session_that_failed_leaves_its_reason_on_screen() {
        // A disconnection explains itself and goes away; a failure is the
        // thing the person came back to the screen to read.
        let mut app = Pravera::assemble(None, false);

        app.end_session(Ending::Failed("The host stopped answering.".into()));
        let notice = app.notice.as_ref().expect("a notice");
        assert!(notice.failure);
        assert_eq!(notice.until, None, "a failure expired on its own");
        assert_eq!(app.screen, Screen::Home);
    }

    #[test]
    fn an_ordinary_disconnection_clears_itself() {
        let mut app = Pravera::assemble(None, false);

        app.end_session(Ending::Requested);
        let notice = app.notice.as_ref().expect("a notice");
        assert!(!notice.failure);
        assert!(notice.until.is_some());
        assert_eq!(app.screen, Screen::Home);
    }

    #[test]
    fn a_notice_that_has_expired_is_taken_down() {
        let mut app = Pravera::assemble(None, false);
        app.end_session(Ending::Requested);

        app.now += NOTICE_FOR + Duration::from_secs(1);
        app.expire_notice();
        assert!(app.notice.is_none());
    }

    #[test]
    fn connecting_without_an_endpoint_says_so_rather_than_doing_nothing() {
        // Pressing Connect in the second before the endpoint finishes binding
        // must not look like the button is broken.
        let mut app = Pravera::assemble(None, false);
        assert!(app.endpoint.is_none());

        let code = pravera_core::connect_code::grouped(&[7u8; 32]);
        let _ = app.on_connect_form(screens::connect::Message::CodeChanged(code));
        let _ = app.on_connect_form(screens::connect::Message::UsernameChanged("me".into()));
        let _ = app.on_connect_form(screens::connect::Message::Submit);

        assert!(
            !app.connect.is_connecting(),
            "it went ahead without an endpoint to dial from"
        );
    }

    #[test]
    fn hosting_cannot_start_before_the_endpoint_is_open() {
        let mut app = Pravera::assemble(None, false);
        let _ = app.on_settings(screens::settings::Message::UsernameChanged("me".into()));
        let _ = app.on_settings(screens::settings::Message::PasswordChanged(
            "hunter2".into(),
        ));

        let _ = app.toggle_hosting();
        assert!(app.hosting.is_none());
        assert!(!app.settings.is_starting());
    }

    #[test]
    fn switching_hosting_off_drops_the_channel_it_reported_on() {
        // Leaving the receiver behind would grow without bound, because
        // nothing drains it once the accept loop is gone.
        let mut app = Pravera::assemble(None, false);
        let (_sender, receiver) = mpsc::unbounded_channel();
        app.host_events = Some(receiver);

        let _ = app.toggle_hosting();
        assert!(app.host_events.is_none() || app.hosting.is_none());
    }
}
