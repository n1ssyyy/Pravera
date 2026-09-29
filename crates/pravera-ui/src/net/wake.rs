//! Waking the interface when a terminal prints.
//!
//! The window only redraws when something asks it to. A video session asks
//! every frame, because every frame has a new picture; a shell mostly sits
//! still, and redrawing sixty times a second to find out whether it printed
//! anything is a GPU spinning for nothing.
//!
//! So each terminal's output is relayed through a task that rings a bell on
//! the way past, and the application listens for the bell as a subscription.
//! Rings coalesce: a burst of output is one wake-up, and the drain that
//! follows takes the whole burst.

use std::sync::LazyLock;

use iced::futures::Stream;
use pravera_client::TerminalEvent;
use tokio::sync::{mpsc, Notify};

static BELL: LazyLock<Notify> = LazyLock::new(Notify::new);

/// Ring the bell. Cheap, and never lost: with nobody waiting, one ring is
/// held until somebody is.
pub fn ring() {
    BELL.notify_one();
}

/// Put a terminal's output on a receiver that rings the bell whenever
/// something arrives on it — and once more when the terminal's pump goes,
/// so a connection that drops is noticed without a key being pressed.
///
/// Must be called inside the Tokio runtime.
pub fn relay(
    mut output: mpsc::UnboundedReceiver<TerminalEvent>,
) -> mpsc::UnboundedReceiver<TerminalEvent> {
    let (events, relayed) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(event) = output.recv().await {
            if events.send(event).is_err() {
                return;
            }
            ring();
        }
        drop(events);
        ring();
    });
    relayed
}

/// One item per wake-up, for as long as the application runs.
pub fn rings() -> impl Stream<Item = ()> {
    iced::futures::stream::unfold((), |()| async {
        BELL.notified().await;
        Some(((), ()))
    })
}
