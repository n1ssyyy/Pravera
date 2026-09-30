//! The cursor stream: host to viewer, one direction, protocol version 3.
//!
//! Carries [`HostMessage::CursorShape`] and [`HostMessage::Cursor`] and nothing
//! else. It is a stream of its own rather than part of the control stream
//! because the control stream is strictly request and response: a reply is
//! matched to its request by order, so a message the host sent unprompted, at
//! the rate a mouse moves, would be read as somebody's answer.
//!
//! It is a reliable stream rather than datagrams on purpose. A lost shape would
//! leave every later position naming an image the viewer never received, and the
//! host already coalesces positions, so the stream carries the latest state
//! rather than a history that could pile up behind a slow link.

use iroh::endpoint::{RecvStream, SendStream};
use pravera_proto::HostMessage;

use crate::control::{read_framed, write_framed};
use crate::error::Result;

/// Host end of the cursor stream.
pub struct CursorSender {
    send: SendStream,
}

impl CursorSender {
    pub(crate) fn new(send: SendStream) -> Self {
        CursorSender { send }
    }

    /// Send one message. Only cursor messages belong here; the viewer ignores
    /// anything else it reads.
    pub async fn send(&mut self, message: &HostMessage) -> Result<()> {
        write_framed(&mut self.send, message).await
    }
}

/// Viewer end of the cursor stream.
pub struct CursorReceiver {
    recv: RecvStream,
}

impl CursorReceiver {
    pub(crate) fn new(recv: RecvStream) -> Self {
        CursorReceiver { recv }
    }

    /// Read the next message. [`crate::TransportError::StreamClosed`] once the
    /// host stops sending.
    pub async fn recv(&mut self) -> Result<HostMessage> {
        read_framed(&mut self.recv).await
    }
}
