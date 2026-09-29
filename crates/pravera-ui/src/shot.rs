//! Pictures of every screen, taken from a window nobody sees.
//!
//! `PRAVERA_PREVIEW=<screen> PRAVERA_SHOT=<dir>` on a debug build opens the
//! preview in a window that is never shown, visits each screen in turn, waits
//! for its entrance to settle, and writes what the renderer drew to
//! `<dir>/<n>-<screen>.png`, then exits. It exists so the interface can be
//! looked at, and compared before and after a change, without a window
//! appearing over whatever the person at the machine is doing.
//!
//! `PRAVERA_SHOT_SIZE=<w>x<h>` picks the window size; the default is the
//! app's own.

use std::path::PathBuf;
use std::time::Duration;

use iced::{Size, Task};

use crate::screens::settings::Section;
use crate::{Message, Screen};

/// Long enough for every entrance and stagger to have finished.
pub const SETTLE: Duration = Duration::from_millis(1400);

/// One picture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// A screen at rest.
    Screen(Screen),
    /// The connect dialog over the device list, which is not a screen of its
    /// own.
    Connect,
    /// The device list with the pointer resting on its first row, to see what
    /// a row shows only under it.
    Hover,
    /// Settings with one of its sections chosen.
    Section(Section),
    /// A transition caught part way, rather than after it has settled.
    Caught(Moment),
}

/// A transition, and how far into it the picture is taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Moment {
    /// Settings to Users, while the old page is still fading out.
    Leaving,
    /// Users to Transfers, once the new page has begun to rise in.
    Arriving,
    /// The connect dialog over Devices, just after it began to open.
    Dialog,
    /// Devices to Settings, with the rail's highlight on its way down.
    Rail,
}

impl Moment {
    /// How long after the change the picture is taken.
    pub const fn wait(self) -> Duration {
        Duration::from_millis(match self {
            Moment::Leaving => 90,
            Moment::Arriving => 150,
            Moment::Dialog => 120,
            Moment::Rail => 60,
        })
    }
}

/// What gets photographed, in order.
pub const PLAN: [Step; 15] = [
    Step::Screen(Screen::Home),
    Step::Connect,
    Step::Screen(Screen::Transfers),
    Step::Screen(Screen::Users),
    Step::Screen(Screen::Mcp),
    Step::Screen(Screen::Settings),
    Step::Hover,
    Step::Section(Section::Displays),
    Step::Section(Section::Unattended),
    Step::Section(Section::Updates),
    Step::Section(Section::Machine),
    Step::Caught(Moment::Leaving),
    Step::Caught(Moment::Arriving),
    Step::Caught(Moment::Dialog),
    Step::Caught(Moment::Rail),
];

/// Where the pictures go, when this run is taking them.
pub fn dir() -> Option<PathBuf> {
    if !super::preview() {
        return None;
    }
    std::env::var_os("PRAVERA_SHOT").map(PathBuf::from)
}

pub fn size() -> Option<Size> {
    if !super::preview() {
        return None;
    }
    let wanted = std::env::var("PRAVERA_SHOT_SIZE").ok()?;
    let (width, height) = wanted.trim().split_once('x')?;
    Some(Size::new(width.parse().ok()?, height.parse().ok()?))
}

/// Put a window where nobody can see it, but where the desktop still draws it.
///
/// A window created with `visible: false` is never painted, so a picture of it
/// is only the clear colour. This shows it far outside every monitor, at the
/// bottom of the stack, without activating it and off the taskbar, so nothing
/// appears on the desktop and nothing takes focus.
pub fn park<T: Send + 'static>(window: iced::window::Id) -> Task<T> {
    iced::window::run(window, |handle| {
        #[cfg(windows)]
        park_window(handle);
        #[cfg(not(windows))]
        let _ = handle;
    })
    .discard()
}

#[cfg(windows)]
fn park_window(handle: &dyn raw_window_handle::HasWindowHandle) {
    use raw_window_handle::RawWindowHandle;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{
        GetWindowLongPtrW, SetWindowLongPtrW, SetWindowPos, GWL_EXSTYLE, HWND_BOTTOM, SWP_NOACTIVATE, SWP_NOSIZE,
        SWP_SHOWWINDOW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    };

    let Ok(raw) = handle.window_handle().map(|handle| handle.as_raw()) else {
        return;
    };
    let RawWindowHandle::Win32(win32) = raw else {
        return;
    };
    let window = HWND(win32.hwnd.get() as *mut _);
    // SAFETY: plain Win32 calls on a window this process owns, made on the
    // thread that owns it.
    unsafe {
        let style = GetWindowLongPtrW(window, GWL_EXSTYLE);
        SetWindowLongPtrW(window, GWL_EXSTYLE, style | (WS_EX_NOACTIVATE.0 | WS_EX_TOOLWINDOW.0) as isize);
        let _ = SetWindowPos(
            window,
            Some(HWND_BOTTOM),
            -32000,
            -32000,
            0,
            0,
            SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW,
        );
    }
}

pub fn label(step: Step) -> &'static str {
    match step {
        Step::Screen(screen) => screen.label(),
        Step::Connect => "Connect",
        Step::Hover => "Hover",
        Step::Section(Section::Hosting) => "Settings-hosting",
        Step::Section(Section::Displays) => "Settings-displays",
        Step::Section(Section::Unattended) => "Settings-unattended",
        Step::Section(Section::Updates) => "Settings-updates",
        Step::Section(Section::Machine) => "Settings-machine",
        Step::Caught(Moment::Leaving) => "Mid-leaving",
        Step::Caught(Moment::Arriving) => "Mid-arriving",
        Step::Caught(Moment::Dialog) => "Mid-dialog",
        Step::Caught(Moment::Rail) => "Mid-rail",
    }
}

/// Wait for the page to settle, then take its picture.
pub fn after_settling(step: usize) -> Task<Message> {
    Task::perform(tokio::time::sleep(SETTLE), move |_| Message::ShotTake(step))
}

/// Wait for the page to settle, then make the change whose middle is to be
/// photographed.
pub fn before_firing(step: usize) -> Task<Message> {
    Task::perform(tokio::time::sleep(SETTLE), move |_| Message::ShotFire(step))
}

/// Wait the moment out, then take the picture.
pub fn after_moment(step: usize, moment: Moment) -> Task<Message> {
    Task::perform(tokio::time::sleep(moment.wait()), move |_| Message::ShotTake(step))
}

pub fn take(step: usize) -> Task<Message> {
    iced::window::latest()
        .and_then(iced::window::screenshot)
        .map(move |picture| Message::ShotTaken(step, picture))
}

/// Write one picture. Failures are printed rather than fatal: one missing
/// picture should not cost the rest.
pub fn save(step: usize, picture: &iced::window::Screenshot) {
    let Some(dir) = dir() else { return };
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join(format!(
        "{}-{}.png",
        step,
        label(PLAN[step]).to_lowercase()
    ));
    let saved = image::RgbaImage::from_raw(
        picture.size.width,
        picture.size.height,
        picture.rgba.to_vec(),
    )
    .ok_or_else(|| "the picture's size does not match its pixels".to_string())
    .and_then(|image| image.save(&path).map_err(|error| error.to_string()));
    match saved {
        Ok(()) => println!("shot {}", path.display()),
        Err(error) => eprintln!("shot {} failed: {error}", path.display()),
    }
}
