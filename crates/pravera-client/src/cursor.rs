//! The host's cursor, received.
//!
//! From protocol version 3 the host sends its pointer separately from the
//! picture (see `pravera_host::cursor`), on a stream of its own. This module
//! reads that stream into a small shared state the interface can look at
//! whenever it draws: the images seen so far, keyed by id, and where the pointer
//! is now. It is pull-based on purpose. The host may send a position a hundred
//! times a second and the interface draws at the display's rate, so only the
//! latest matters and there is nothing to queue.
//!
//! A version 2 host never opens the stream, so [`Client::cursor`] is `None` for
//! it and the interface behaves as it did before.
//!
//! [`Client::cursor`]: crate::Client::cursor

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;
use pravera_proto::HostMessage;
use pravera_transport::{Session, TransportError};
use tokio::sync::Notify;
use tracing::{debug, warn};

/// Images kept before the host is judged to be misbehaving. Each is at most
/// 96x96 RGBA, so this bounds the cache at about 9 MB however hostile the peer.
const MAX_SHAPES: usize = 256;

/// One cursor image, straight RGBA.
#[derive(Debug, PartialEq, Eq)]
pub struct CursorImage {
    pub id: u32,
    pub width: u16,
    pub height: u16,
    /// The pixel that sits on the pointer position.
    pub hot_x: u16,
    pub hot_y: u16,
    pub rgba: Vec<u8>,
}

/// Where the host's pointer is and what it looks like.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorState {
    /// In pixels of the streamed picture.
    pub x: i32,
    pub y: i32,
    /// False when the pointer is on another display or an application hid it.
    pub visible: bool,
    /// `None` when the host named a shape it never sent, which a well-behaved
    /// host does not do.
    pub image: Option<Arc<CursorImage>>,
}

#[derive(Default)]
struct Shared {
    shapes: HashMap<u32, Arc<CursorImage>>,
    state: Option<CursorState>,
    /// Rises whenever `state` changes, so a caller can tell "nothing new" from
    /// "the same again" without comparing images.
    serial: u64,
    ended: bool,
}

/// The receiving end of the cursor stream.
pub struct CursorStream {
    shared: Arc<Mutex<Shared>>,
    changed: Arc<Notify>,
    task: tokio::task::JoinHandle<()>,
}

impl CursorStream {
    /// Start reading the stream. Must be called inside a tokio runtime.
    pub(crate) fn start(session: Session) -> CursorStream {
        let shared = Arc::new(Mutex::new(Shared::default()));
        let changed = Arc::new(Notify::new());
        let task = {
            let (shared, changed) = (shared.clone(), changed.clone());
            tokio::spawn(async move {
                let mut stream = match session.accept_cursor().await {
                    Ok(stream) => stream,
                    Err(error) => {
                        debug!(%error, "no cursor stream");
                        shared.lock().ended = true;
                        changed.notify_one();
                        return;
                    }
                };
                loop {
                    match stream.recv().await {
                        Ok(message) => {
                            if apply(&mut shared.lock(), message) {
                                changed.notify_one();
                            }
                        }
                        Err(TransportError::StreamClosed) => break,
                        Err(error) => {
                            debug!(%error, "the cursor stream ended");
                            break;
                        }
                    }
                }
                shared.lock().ended = true;
                changed.notify_one();
            })
        };
        CursorStream {
            shared,
            changed,
            task,
        }
    }

    /// The pointer as last reported, and a serial that changes when it does.
    /// `None` until the host has sent a position.
    pub fn latest(&self) -> Option<(u64, CursorState)> {
        let shared = self.shared.lock();
        shared.state.clone().map(|state| (shared.serial, state))
    }

    /// Whether the host has stopped sending. A viewer that sees this goes back
    /// to its own cursor.
    pub fn has_ended(&self) -> bool {
        self.shared.lock().ended
    }

    /// Resolves when something changed since it last resolved. Coalescing:
    /// however many updates arrived, one wake-up.
    pub async fn changed(&self) {
        self.changed.notified().await;
    }
}

impl Drop for CursorStream {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Fold one message into the state. Returns whether the caller should redraw.
fn apply(shared: &mut Shared, message: HostMessage) -> bool {
    if !message.is_well_formed() {
        warn!("ignoring a malformed cursor message");
        return false;
    }
    match message {
        HostMessage::CursorShape {
            id,
            width,
            height,
            hot_x,
            hot_y,
            rgba,
        } => {
            if shared.shapes.len() >= MAX_SHAPES && !shared.shapes.contains_key(&id) {
                warn!("the host sent too many cursor shapes; ignoring more");
                return false;
            }
            shared.shapes.insert(
                id,
                Arc::new(CursorImage {
                    id,
                    width,
                    height,
                    hot_x,
                    hot_y,
                    rgba,
                }),
            );
            false
        }
        HostMessage::Cursor {
            x,
            y,
            visible,
            shape,
        } => {
            shared.serial += 1;
            shared.state = Some(CursorState {
                x,
                y,
                visible,
                image: shared.shapes.get(&shape).cloned(),
            });
            true
        }
        // Nothing else belongs on this stream.
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shape(id: u32) -> HostMessage {
        HostMessage::CursorShape {
            id,
            width: 2,
            height: 2,
            hot_x: 1,
            hot_y: 0,
            rgba: vec![0; 16],
        }
    }

    fn moved(x: i32, shape: u32) -> HostMessage {
        HostMessage::Cursor {
            x,
            y: 5,
            visible: true,
            shape,
        }
    }

    #[test]
    fn a_position_carries_the_image_its_shape_id_names() {
        let mut shared = Shared::default();
        assert!(!apply(&mut shared, shape(4)));
        assert!(apply(&mut shared, moved(10, 4)));
        let state = shared.state.unwrap();
        assert_eq!((state.x, state.y, state.visible), (10, 5, true));
        assert_eq!(state.image.unwrap().id, 4);
    }

    #[test]
    fn a_position_naming_an_unknown_shape_has_no_image() {
        let mut shared = Shared::default();
        apply(&mut shared, moved(1, 77));
        assert!(shared.state.unwrap().image.is_none());
    }

    #[test]
    fn every_position_bumps_the_serial() {
        let mut shared = Shared::default();
        apply(&mut shared, shape(0));
        apply(&mut shared, moved(1, 0));
        let first = shared.serial;
        apply(&mut shared, moved(2, 0));
        assert!(shared.serial > first);
    }

    #[test]
    fn a_malformed_shape_is_ignored() {
        let mut shared = Shared::default();
        let bad = HostMessage::CursorShape {
            id: 1,
            width: 4,
            height: 4,
            hot_x: 0,
            hot_y: 0,
            rgba: vec![0; 3],
        };
        assert!(!apply(&mut shared, bad));
        assert!(shared.shapes.is_empty());
    }

    #[test]
    fn the_shape_cache_is_bounded() {
        let mut shared = Shared::default();
        for id in 0..(MAX_SHAPES as u32 + 50) {
            apply(&mut shared, shape(id));
        }
        assert_eq!(shared.shapes.len(), MAX_SHAPES);
    }

    #[test]
    fn messages_that_do_not_belong_here_change_nothing() {
        let mut shared = Shared::default();
        assert!(!apply(&mut shared, HostMessage::Pong { nonce: 1 }));
        assert!(shared.state.is_none());
    }
}
