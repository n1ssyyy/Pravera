//! The reliable control stream.
//!
//! Length-prefixed postcard messages over a QUIC bidirectional stream, using
//! the framing defined in `pravera_proto::codec`.
//!
//! ## Why there are two types instead of one
//!
//! [`ClientControl`] can only send a [`ClientMessage`] and can only receive a
//! [`HostMessage`]; [`HostControl`] is the mirror. The underlying stream is the
//! same, and the split costs nothing at runtime, but it means a host cannot
//! accidentally send a client message: the mistake is a compile error rather
//! than a peer that hangs waiting for a reply it will never recognise.

use pravera_proto::{codec, ClientMessage, HostMessage};
use serde::{de::DeserializeOwned, Serialize};

use iroh::endpoint::{ReadExactError, RecvStream, SendStream};

use crate::error::{Result, TransportError};

/// Write one length-prefixed postcard message.
pub(crate) async fn write_framed<T: Serialize>(send: &mut SendStream, message: &T) -> Result<()> {
    let framed = codec::encode(message)?;
    send.write_all(&framed)
        .await
        .map_err(TransportError::stream)?;
    Ok(())
}

/// Read one length-prefixed postcard message.
///
/// Two exact reads: the four-byte length prefix, then the body. The prefix is
/// checked by `codec::body_length` **before** the body buffer is sized, which
/// is the whole reason the framing has a cap. Without that check this would be
/// a peer-controlled allocation.
pub(crate) async fn read_framed<T: DeserializeOwned>(recv: &mut RecvStream) -> Result<T> {
    let mut prefix = [0u8; codec::LENGTH_PREFIX];
    match recv.read_exact(&mut prefix).await {
        Ok(()) => {}
        // A clean finish with nothing pending is the peer hanging up, not a
        // failure. A finish *partway through* a prefix is a truncated message
        // and belongs in the error path.
        Err(ReadExactError::FinishedEarly(0)) => return Err(TransportError::StreamClosed),
        Err(e) => return Err(TransportError::stream(e)),
    }

    let length = codec::body_length(prefix)?;
    let mut body = vec![0u8; length];
    recv.read_exact(&mut body)
        .await
        .map_err(TransportError::stream)?;

    Ok(codec::decode(&body)?)
}

/// The untyped half. Not public: every caller goes through one of the two
/// typed wrappers below.
pub(crate) struct ControlStream {
    send: SendStream,
    recv: RecvStream,
}

impl ControlStream {
    pub(crate) fn new(send: SendStream, recv: RecvStream) -> Self {
        ControlStream { send, recv }
    }

    async fn write<T: Serialize>(&mut self, message: &T) -> Result<()> {
        write_framed(&mut self.send, message).await
    }

    async fn read<T: DeserializeOwned>(&mut self) -> Result<T> {
        read_framed(&mut self.recv).await
    }

    /// Signal that nothing further will be sent on this side.
    ///
    /// Returns as soon as the FIN is queued. It does not wait for the peer, so
    /// finishing and then closing the connection can still lose the last
    /// message; see [`ControlStream::flush`].
    fn finish(&mut self) -> Result<()> {
        self.send.finish().map_err(TransportError::stream)
    }

    /// Finish, and wait until the peer has acknowledged every byte.
    ///
    /// `Connection::close` immediately abandons undelivered data, so a host
    /// that sends a final reply and closes in the same breath usually loses
    /// that reply. The two messages where this matters most are a refused login
    /// and a version mismatch: each is the last thing the host says and the
    /// whole reason the peer connected, and without this the client sees only
    /// an unexplained disconnect.
    ///
    /// Acknowledgement is a transport fact, not an application one: it means
    /// the bytes arrived, not that anyone read them. That is the strongest
    /// guarantee available without an application-level goodbye handshake, and
    /// it is enough to make the difference between "denied" and "the connection
    /// dropped".
    async fn flush(&mut self) -> Result<()> {
        self.finish()?;
        self.send.stopped().await.map_err(TransportError::stream)?;
        Ok(())
    }
}

/// The client's end of the control stream.
pub struct ClientControl {
    inner: ControlStream,
}

impl ClientControl {
    pub(crate) fn new(inner: ControlStream) -> Self {
        ClientControl { inner }
    }

    pub async fn send(&mut self, message: &ClientMessage) -> Result<()> {
        self.inner.write(message).await
    }

    pub async fn recv(&mut self) -> Result<HostMessage> {
        self.inner.read().await
    }

    /// Send a message and wait for the reply. The whole handshake is this
    /// shape, so it is worth having once rather than six times.
    pub async fn request(&mut self, message: &ClientMessage) -> Result<HostMessage> {
        self.send(message).await?;
        self.recv().await
    }

    pub fn finish(&mut self) -> Result<()> {
        self.inner.finish()
    }

    /// Finish and wait for the peer to acknowledge. See
    /// [`ControlStream::flush`].
    pub async fn flush(&mut self) -> Result<()> {
        self.inner.flush().await
    }
}

/// The host's end of the control stream.
pub struct HostControl {
    inner: ControlStream,
}

impl HostControl {
    pub(crate) fn new(inner: ControlStream) -> Self {
        HostControl { inner }
    }

    pub async fn send(&mut self, message: &HostMessage) -> Result<()> {
        self.inner.write(message).await
    }

    pub async fn recv(&mut self) -> Result<ClientMessage> {
        self.inner.read().await
    }

    pub fn finish(&mut self) -> Result<()> {
        self.inner.finish()
    }

    /// Finish and wait for the peer to acknowledge. See
    /// [`ControlStream::flush`].
    pub async fn flush(&mut self) -> Result<()> {
        self.inner.flush().await
    }
}
