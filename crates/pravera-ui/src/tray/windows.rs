//! The Windows notification-area icon.
//!
//! `tray-icon` puts its own hidden window on whichever thread creates it and
//! expects that thread's message loop to pump it. iced's event loop is that
//! loop, and it runs on the main thread, so the icon is created on the main
//! thread before iced starts and lives until the process ends.
//!
//! ## Pushed, not polled
//!
//! Clicks could be read off two process-wide channels with `try_recv`, which
//! would mean waking the whole application several times a second forever on
//! the chance that somebody clicked. Instead the handlers `tray-icon` offers
//! are installed at creation and forward into a channel the interface
//! subscribes to, so a Pravera with nothing to do costs nothing at all until
//! the moment somebody clicks, and then responds immediately.
//!
//! The handlers run on whichever thread processed the window message — this
//! one — and do nothing but send. Everything that reads state happens back in
//! the update loop.

use std::sync::Mutex;

use iced::futures::{stream, Stream};
use tokio::sync::mpsc;
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};

use super::{icon, Request, Status};

/// Where [`Tray::new`] leaves the receiving end for the subscription to pick
/// up.
///
/// A process-global is the honest shape here: there is exactly one
/// notification area, one icon in it, and one set of handlers that `tray-icon`
/// will accept — all three are already global whether this says so or not.
static REQUESTS: Mutex<Option<mpsc::UnboundedReceiver<Request>>> = Mutex::new(None);

/// The notification-area icon and its menu.
pub struct Tray {
    icon: tray_icon::TrayIcon,
    hosting: MenuItem,
    /// What the icon was last told, so a tick that changes nothing does not
    /// rewrite the tooltip and the menu sixty times a second.
    shown: Option<(Option<String>, bool, u64)>,
}

impl Tray {
    /// Put the icon in the notification area.
    ///
    /// `None` if it could not be created — a session with no shell, a
    /// notification area that refuses another icon. That is not fatal: the
    /// window still works, and closing it quits rather than hiding, which is
    /// the right behaviour when there would be nothing left to click.
    pub fn new(status: &Status) -> Option<Tray> {
        let menu = Menu::new();

        let show = MenuItem::new("Open Pravera", true, None);
        let hosting = MenuItem::new(status.hosting_label(), true, None);
        let quit = MenuItem::new("Quit Pravera", true, None);

        menu.append_items(&[
            &show,
            &PredefinedMenuItem::separator(),
            &hosting,
            &PredefinedMenuItem::separator(),
            &quit,
        ])
        .ok()?;

        let image = Icon::from_rgba(icon::pixels(status.hosting), icon::size(), icon::size())
            .map_err(|error| tracing::warn!(%error, "the tray icon could not be drawn"))
            .ok()?;

        let icon = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip(status.tooltip())
            .with_icon(image)
            // The left click opens the window, which is what a person expects
            // and what the menu's first entry does anyway.
            .with_menu_on_left_click(false)
            .build()
            .map_err(|error| tracing::warn!(%error, "the notification area refused an icon"))
            .ok()?;

        let (sender, receiver) = mpsc::unbounded_channel();
        if let Ok(mut slot) = REQUESTS.lock() {
            *slot = Some(receiver);
        }

        // Compared by id rather than by label, because the hosting entry's
        // label changes with the state it describes.
        let (show, toggle, quit) = (show.id().clone(), hosting.id().clone(), quit.id().clone());
        let menu_clicks = sender.clone();
        MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
            let request = if event.id == show {
                Request::Show
            } else if event.id == toggle {
                Request::ToggleHosting
            } else if event.id == quit {
                Request::Quit
            } else {
                return;
            };
            let _ = menu_clicks.send(request);
        }));

        TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
            // A left button *release*. Acting on the press would open the
            // window from a click that was on its way to becoming a drag.
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                let _ = sender.send(Request::Show);
            }
        }));

        Some(Tray {
            icon,
            hosting,
            shown: None,
        })
    }

    /// Bring the icon up to date.
    pub fn update(&mut self, status: &Status) {
        let now = (status.device.clone(), status.hosting, status.connections);
        if self.shown.as_ref() == Some(&now) {
            return;
        }

        // Only redraw the image when the state it depicts changed: the glyph
        // is generated, and generating it per connection count would be waste.
        let hosting_changed = self.shown.as_ref().map(|(_, h, _)| *h) != Some(status.hosting);
        if hosting_changed {
            self.hosting.set_text(status.hosting_label());
            if let Ok(image) =
                Icon::from_rgba(icon::pixels(status.hosting), icon::size(), icon::size())
            {
                let _ = self.icon.set_icon(Some(image));
            }
        }

        let _ = self.icon.set_tooltip(Some(status.tooltip()));
        self.shown = Some(now);
    }
}

/// Everything clicked in the notification area, as it happens.
///
/// Ends, rather than yielding nothing forever, if the receiver has already
/// been taken — which would mean this was subscribed to twice, and a second
/// stream that silently never produced anything would be much harder to
/// notice than one that stops.
pub fn requests() -> impl Stream<Item = Request> {
    let receiver = REQUESTS.lock().ok().and_then(|mut slot| slot.take());

    stream::unfold(receiver, |receiver| async move {
        let mut receiver = receiver?;
        let request = receiver.recv().await?;
        Some((request, Some(receiver)))
    })
}
