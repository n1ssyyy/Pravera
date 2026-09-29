//! The notification-area icon, and the window that hides behind it.
//!
//! A machine with no monitor still has to be reachable, which means Pravera has
//! to run with no window on screen and no one signed in front of it. The tray
//! icon is what makes that state visible rather than secret: something is
//! accepting connections on this machine, here is what it is, and here is how
//! to stop it.
//!
//! ## Not a background process
//!
//! Pravera deliberately does not run with nothing to show for it. Closing the
//! window while hosting leaves the icon; closing it while not hosting quits.
//! There is no arrangement where the app is running, reachable, and invisible —
//! a remote-access tool that can be running without any sign of it is a remote
//! access tool that has been installed on someone rather than by them.
//!
//! ## Windows only, for now
//!
//! `tray-icon` reaches the Linux notification area through libappindicator and
//! a GTK main loop, and iced already owns the only main loop this process has.
//! Making those two coexist is real work with a real chance of deadlocking the
//! interface, and it buys nothing on Windows, which is where the machine this
//! was asked for runs. [`Tray::new`] returns `None` everywhere else and the
//! window simply closes normally.

#[cfg_attr(windows, path = "windows.rs")]
#[cfg_attr(not(windows), path = "unsupported.rs")]
mod platform;

pub use platform::Tray;

pub mod icon;

/// Everything clicked in the notification area.
///
/// Only worth subscribing to once [`Tray::new`] has returned an icon: the
/// stream carries what that icon's own handlers report, and there is exactly
/// one of it per process.
pub fn subscription() -> iced::Subscription<Request> {
    iced::Subscription::run(platform::requests)
}

/// What someone asked for by clicking the icon or its menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Request {
    /// Bring the window back.
    Show,
    /// Start hosting if it is off, stop it if it is on.
    ToggleHosting,
    /// Really quit, window and hosting together.
    Quit,
}

/// What the icon should currently say.
#[derive(Debug, Clone)]
pub struct Status {
    /// This machine's device ID, so the tooltip names which machine this is.
    /// `None` before the endpoint has finished binding.
    pub device: Option<String>,
    pub hosting: bool,
    pub connections: u64,
}

impl Status {
    /// The tooltip, which is the only place the state is legible when there is
    /// no window.
    pub fn tooltip(&self) -> String {
        let machine = match &self.device {
            Some(device) => format!("Pravera · {device}"),
            None => "Pravera".to_string(),
        };

        let state = match (self.hosting, self.connections) {
            (false, _) => "not accepting sessions".to_string(),
            (true, 0) => "accepting sessions".to_string(),
            (true, 1) => "1 machine connected".to_string(),
            (true, n) => format!("{n} machines connected"),
        };

        format!("{machine}\n{state}")
    }

    /// What the hosting entry in the menu should read.
    pub fn hosting_label(&self) -> &'static str {
        if self.hosting {
            "Stop hosting"
        } else {
            "Start hosting"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(device: Option<&str>, hosting: bool, connections: u64) -> Status {
        Status {
            device: device.map(str::to_string),
            hosting,
            connections,
        }
    }

    #[test]
    fn the_tooltip_names_the_machine_and_says_whether_it_is_reachable() {
        // With no window, this is the only thing that answers either question.
        let shown = status(Some("PRV-6RND-RZD4"), true, 0).tooltip();
        assert!(shown.contains("PRV-6RND-RZD4"), "{shown}");
        assert!(shown.contains("accepting sessions"), "{shown}");
    }

    #[test]
    fn a_machine_being_watched_says_so_in_the_tooltip() {
        // Somebody is looking at this screen right now. That is worth being
        // able to see without opening anything.
        let shown = status(Some("PRV-6RND-RZD4"), true, 2).tooltip();
        assert!(shown.contains("2 machines connected"), "{shown}");

        let one = status(Some("PRV-6RND-RZD4"), true, 1).tooltip();
        assert!(one.contains("1 machine connected"), "{one}");
    }

    #[test]
    fn a_machine_that_is_not_hosting_never_reads_as_reachable() {
        let shown = status(Some("PRV-6RND-RZD4"), false, 0).tooltip();
        assert!(shown.contains("not accepting sessions"), "{shown}");
    }

    #[test]
    fn the_tooltip_works_before_the_endpoint_has_a_name() {
        let shown = status(None, false, 0).tooltip();
        assert!(shown.starts_with("Pravera"), "{shown}");
        assert!(!shown.contains("None"), "{shown}");
    }

    #[test]
    fn the_menu_entry_says_what_pressing_it_will_do() {
        assert_eq!(status(None, false, 0).hosting_label(), "Start hosting");
        assert_eq!(status(None, true, 0).hosting_label(), "Stop hosting");
    }
}
