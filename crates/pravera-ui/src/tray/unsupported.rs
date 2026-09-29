//! No notification area here.
//!
//! Reaching the Linux one means libappindicator and a GTK main loop, and iced
//! already owns the only main loop this process has. Rather than pretend, this
//! refuses to create an icon, and the interface falls back to closing the
//! window normally — which is honest: there would be nothing left to click.
//!
//! Until this exists, unattended hosting on Linux is `pravera-service` (P5),
//! which is where it belongs anyway: a systemd unit survives a reboot and a
//! logout, and a tray icon does neither.

use iced::futures::{stream, Stream};

use super::{Request, Status};

pub struct Tray;

impl Tray {
    pub fn new(_status: &Status) -> Option<Tray> {
        tracing::debug!("no notification area on this platform");
        None
    }

    pub fn update(&mut self, _status: &Status) {}
}

/// Nothing, forever. Never subscribed to, because [`Tray::new`] never returns
/// one to subscribe on behalf of.
pub fn requests() -> impl Stream<Item = Request> {
    stream::pending()
}
