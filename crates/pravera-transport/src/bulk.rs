//! Streams for moving bulk data.
//!
//! One per transfer, opened when it starts and gone when it ends. Everything
//! file-related uses one: directory listings as well as file bodies, because
//! both are large enough to be felt on the stream that carries the pointer.
//!
//! ## Why not just use the control stream
//!
//! QUIC streams are independent. Bytes queued on one do not delay bytes on
//! another, and loss on one does not stall the other — that is the whole reason
//! QUIC has streams rather than one ordered pipe. A 40 GB copy and a keystroke
//! sent at the same moment arrive independently.
//!
//! Opening one costs nothing: there is no handshake, the stream begins with its
//! first byte, and the request travels in that same flight. A transfer is one
//! round trip from asking to receiving, the same as the control stream would
//! have been.
//!
//! ## Framed messages, then raw bytes
//!
//! A bulk stream carries a short framed conversation — a request, a reply —
//! and then, for a transfer, a body of exactly the length the reply named
//! followed by its hash. The body is *not* framed: framing it would mean
//! either one enormous message the receiver has to hold entire, or a
//! per-chunk header for no benefit over what QUIC already does.
//!
//! Both ends know the length before the body starts, so the receiver reads
//! exactly that many bytes and stops. The length is never used to size a
//! buffer.

use serde::{de::DeserializeOwned, Serialize};

use iroh::endpoint::{ReadExactError, RecvStream, SendStream, VarInt};

use crate::control::{read_framed, write_framed};
use crate::error::{Result, TransportError};

/// Told to the peer when a transfer is cancelled from this end.
///
/// QUIC carries an application code on a reset, and a sender that reads it
/// knows the difference between "the receiver went away" and "the receiver
/// deliberately stopped". Nothing else in Pravera resets a stream, so the value
/// is arbitrary; what matters is that it is not zero, which is the code for an
/// ordinary finish.
const CANCELLED: u32 = 1;

/// One bidirectional stream, carrying framed messages and then raw bytes.
pub struct BulkStream {
    send: SendStream,
    recv: RecvStream,
}

impl BulkStream {
    pub(crate) fn new(send: SendStream, recv: RecvStream) -> BulkStream {
        BulkStream { send, recv }
    }

    /// Send one framed message.
    pub async fn send<T: Serialize>(&mut self, message: &T) -> Result<()> {
        write_framed(&mut self.send, message).await
    }

    /// Read one framed message.
    ///
    /// A peer that hangs up before sending one gives
    /// [`TransportError::StreamClosed`], which for a bulk stream is an ordinary
    /// ending rather than a failure: it is how a cancelled transfer looks from
    /// the other side.
    pub async fn recv<T: DeserializeOwned>(&mut self) -> Result<T> {
        read_framed(&mut self.recv).await
    }

    /// Write raw body bytes.
    pub async fn write_all(&mut self, bytes: &[u8]) -> Result<()> {
        self.send
            .write_all(bytes)
            .await
            .map_err(TransportError::stream)
    }

    /// Read exactly `buffer.len()` body bytes.
    ///
    /// A peer that stops early is [`TransportError::StreamClosed`] rather than
    /// a stream error, because a transfer that was cancelled halfway is not a
    /// broken connection and must not be reported as one.
    pub async fn read_exact(&mut self, buffer: &mut [u8]) -> Result<()> {
        match self.recv.read_exact(buffer).await {
            Ok(()) => Ok(()),
            Err(ReadExactError::FinishedEarly(_)) => Err(TransportError::StreamClosed),
            Err(e) => Err(TransportError::stream(e)),
        }
    }

    /// Finish sending, and wait for the peer to acknowledge every byte.
    ///
    /// The wait is what makes the difference between "the file is on the far
    /// end" and "the file was handed to the network and then the connection
    /// closed". `Connection::close` abandons anything still in flight.
    pub async fn flush(&mut self) -> Result<()> {
        self.send.finish().map_err(TransportError::stream)?;
        self.send.stopped().await.map_err(TransportError::stream)?;
        Ok(())
    }

    /// Stop this transfer now, and tell the peer it was deliberate.
    ///
    /// Both directions: the peer stops sending as well as stops expecting. A
    /// download the person cancelled must not keep costing bandwidth while the
    /// host works through the rest of a large file.
    pub fn cancel(&mut self) {
        let code = VarInt::from_u32(CANCELLED);
        let _ = self.send.reset(code);
        let _ = self.recv.stop(code);
    }
}
