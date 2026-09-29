//! Answering file requests.
//!
//! Runs alongside [`crate::serve`] rather than inside it. The control loop is
//! one task handling one message at a time, which is exactly right for input
//! and exactly wrong for a copy that takes ten minutes.
//!
//! ## The permission mirror
//!
//! [`HostSession`](crate::HostSession) is the only thing that decides what a
//! login may do, and it lives inside the control loop. This module cannot ask
//! it — a channel round trip per request would put the transfers back behind
//! the control stream — so the control loop *publishes* the answer instead, to
//! a [`Gate`].
//!
//! The distinction matters: the gate is written only by the session, from the
//! session's own record of the role, and read here. Nothing a client sends can
//! change it. It is a mirror of the authority, not a second authority — and it
//! starts closed, so a bulk stream opened before the login completes is refused
//! rather than raced.
//!
//! ## Blocking work stays off the async workers
//!
//! Reading a file is a blocking syscall. Doing it on a tokio worker would
//! stall every other task on that thread for the length of the read, and the
//! tasks sharing those threads include the one sending video datagrams. So
//! each transfer runs its disk half on a blocking thread and passes chunks
//! through a small bounded channel, which also means the next disk read
//! overlaps the current network write instead of following it.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use pravera_core::{for_log, Permission};
use pravera_files::{Sink, Source};
use pravera_proto::{FileError, FileReply, FileRequest, Location, HASH_BYTES, TRANSFER_CHUNK};
use pravera_transport::{BulkStream, Result, Session};
use tokio::sync::{mpsc, Semaphore};
use tracing::{debug, info, warn};

/// How many transfers this host will run at once.
///
/// Four saturates any link worth streaming video over, and a fifth would only
/// make the other four slower. Past this, QUIC's own stream limit holds the
/// client back — which is backpressure rather than a refusal, so nothing has to
/// be retried.
const CONCURRENT: usize = 4;

/// How many chunks may sit between the disk and the network.
///
/// Enough that the next read overlaps the current write; small enough that a
/// transfer's memory cost stays a few hundred kilobytes rather than growing
/// with the file.
const IN_FLIGHT: usize = 4;

/// What the session has granted, readable without asking it.
///
/// See the module documentation: this is a mirror of the session's decision,
/// never a second one. Cloning shares the same value, which is the point —
/// every transfer sees the current grant rather than the one that held when it
/// started.
#[derive(Clone, Default)]
pub struct Gate(Arc<AtomicU32>);

impl Gate {
    /// A gate that allows nothing. The state before anyone has logged in.
    pub fn closed() -> Gate {
        Gate(Arc::new(AtomicU32::new(0)))
    }

    /// Publish what the session granted. Called only by the control loop.
    pub(crate) fn open(&self, permissions: Permission) {
        self.0.store(permissions.bits(), Ordering::Release);
    }

    /// Revoke everything. Called when the session ends, so a transfer that
    /// outlives its connection stops rather than finishing on its own.
    pub(crate) fn close(&self) {
        self.0.store(0, Ordering::Release);
    }

    /// Whether the current grant covers `needed`.
    pub fn allows(&self, needed: Permission) -> bool {
        Permission::from_bits_truncate(self.0.load(Ordering::Acquire)).contains(needed)
    }
}

/// Accept and answer file requests until the connection ends.
///
/// **The control stream must already be established** before this is called.
/// QUIC delivers accepted streams in the order the peer opened them, and that
/// ordering is the only thing separating the control stream from the first bulk
/// one — start accepting here too early and this takes the control stream for a
/// transfer.
pub async fn serve_files(session: Session, gate: Gate) {
    let slots = Arc::new(Semaphore::new(CONCURRENT));

    loop {
        let stream = match session.accept_bulk().await {
            Ok(stream) => stream,
            // The connection ended. Ordinary: it is how every session finishes.
            Err(error) => {
                debug!(%error, "no more file streams");
                return;
            }
        };

        // Awaited *after* accepting, so a client past the limit waits on QUIC's
        // own stream credit rather than being refused something it could just
        // retry.
        let Ok(slot) = slots.clone().acquire_owned().await else {
            return;
        };
        let gate = gate.clone();
        tokio::spawn(async move {
            if let Err(error) = answer(stream, gate).await {
                debug!(%error, "a file stream ended early");
            }
            drop(slot);
        });
    }
}

/// Handle one bulk stream: one request, one answer, and whatever body follows.
async fn answer(mut stream: BulkStream, gate: Gate) -> Result<()> {
    let request: FileRequest = stream.recv().await?;

    // Permission first, shape second. A role that may not touch files learns
    // nothing about whether its path was well-formed.
    if !gate.allows(request.required_permission()) {
        return refuse(&mut stream, FileError::NotPermitted).await;
    }
    if !request.is_well_formed() {
        warn!(
            path = request.path().map(for_log),
            "refused a file request whose path was not one a listing could have produced"
        );
        return refuse(&mut stream, FileError::Malformed).await;
    }

    match request {
        FileRequest::List(location) => list(&mut stream, location).await,
        FileRequest::Download { path } => download(&mut stream, path).await,
        FileRequest::Upload {
            path,
            size,
            replace,
        } => upload(&mut stream, path, size, replace, &gate).await,
    }
}

async fn refuse(stream: &mut BulkStream, error: FileError) -> Result<()> {
    stream.send(&FileReply::Refused(error)).await?;
    stream.flush().await
}

async fn list(stream: &mut BulkStream, location: Location) -> Result<()> {
    // `read_dir` on a cold or network directory blocks for as long as it takes.
    let listed = tokio::task::spawn_blocking(move || pravera_files::browse(&location))
        .await
        .unwrap_or(Err(FileError::Unreadable));

    match listed {
        Ok(listing) => {
            debug!(
                entries = listing.entries.len(),
                truncated = listing.truncated,
                "listed a directory"
            );
            stream.send(&FileReply::Listing(listing)).await?;
            stream.flush().await
        }
        Err(error) => refuse(stream, error).await,
    }
}

/// One piece of a file on its way out.
enum Piece {
    Body(Vec<u8>),
    /// Everything has been read, and this is what it hashed to.
    Done([u8; HASH_BYTES]),
    Failed(FileError),
}

async fn download(stream: &mut BulkStream, path: String) -> Result<()> {
    let opening = path.clone();
    let opened = tokio::task::spawn_blocking(move || Source::open(&opening))
        .await
        .unwrap_or(Err(FileError::Unreadable));

    let source = match opened {
        Ok(source) => source,
        Err(error) => return refuse(stream, error).await,
    };

    let size = source.size();
    let modified = source.modified();
    stream.send(&FileReply::Sending { size, modified }).await?;
    info!(bytes = size, path = %for_log(&path), "sending a file");

    let (pieces, mut incoming) = mpsc::channel(IN_FLIGHT);
    tokio::task::spawn_blocking(move || read_into(source, pieces));

    // The body, then the trailer. A failure partway through resets the stream
    // rather than sending a short body: the receiver is counting bytes, and a
    // refusal in the middle of them would be read as file content.
    while let Some(piece) = incoming.recv().await {
        match piece {
            Piece::Body(bytes) => stream.write_all(&bytes).await?,
            Piece::Done(hash) => {
                stream.write_all(&hash).await?;
                return stream.flush().await;
            }
            Piece::Failed(error) => {
                warn!(%error, path = %for_log(&path), "a file stopped being readable partway through");
                stream.cancel();
                return Ok(());
            }
        }
    }

    // The reader went away without saying how it ended, which means the task
    // was cancelled or the runtime is shutting down.
    stream.cancel();
    Ok(())
}

/// The blocking half of a download: read, hash, hand over.
fn read_into(mut source: Source, pieces: mpsc::Sender<Piece>) {
    let mut buffer = vec![0u8; TRANSFER_CHUNK];
    loop {
        let piece = match source.next(&mut buffer) {
            Ok(0) => Piece::Done(source.hash()),
            Ok(read) => Piece::Body(buffer[..read].to_vec()),
            Err(error) => Piece::Failed(error),
        };
        let last = !matches!(piece, Piece::Body(_));
        // A closed channel means the receiver gave up — a cancelled download or
        // a dropped connection. Stop reading rather than working through the
        // rest of a large file for nobody.
        if pieces.blocking_send(piece).is_err() || last {
            return;
        }
    }
}

async fn upload(
    stream: &mut BulkStream,
    path: String,
    size: u64,
    replace: bool,
    gate: &Gate,
) -> Result<()> {
    let creating = path.clone();
    let created = tokio::task::spawn_blocking(move || Sink::create(&creating, replace))
        .await
        .unwrap_or(Err(FileError::Unwritable));

    let mut sink = match created {
        Ok(sink) => sink,
        Err(error) => return refuse(stream, error).await,
    };

    stream.send(&FileReply::Ready).await?;
    info!(bytes = size, path = %for_log(&path), "receiving a file");

    // Read exactly what was promised, in fixed pieces. `size` never sizes a
    // buffer, so a peer claiming a 900 TB file allocates nothing here.
    let mut buffer = vec![0u8; TRANSFER_CHUNK];
    let mut left = size;
    while left > 0 {
        let want = TRANSFER_CHUNK.min(left as usize);
        stream.read_exact(&mut buffer[..want]).await?;
        left -= want as u64;

        // Re-checked every chunk, not only at the start. A session that ends
        // mid-upload must not go on writing to the host's disk, and neither
        // must one whose role was revoked.
        if !gate.allows(Permission::FILE_WRITE) {
            warn!(path = %for_log(&path), "an upload stopped being permitted partway through");
            stream.cancel();
            return Ok(());
        }

        let piece = buffer[..want].to_vec();
        let written = tokio::task::spawn_blocking(move || {
            sink.write(&piece)?;
            Ok::<Sink, FileError>(sink)
        })
        .await
        .unwrap_or(Err(FileError::Unwritable));

        sink = match written {
            Ok(sink) => sink,
            Err(error) => {
                warn!(%error, path = %for_log(&path), "a file being received could not be written");
                return refuse(stream, error).await;
            }
        };
    }

    let mut expected = [0u8; HASH_BYTES];
    stream.read_exact(&mut expected).await?;

    let stored = tokio::task::spawn_blocking(move || sink.finish(&expected))
        .await
        .unwrap_or(Err(FileError::Unwritable));

    match stored {
        Ok(()) => {
            info!(bytes = size, path = %for_log(&path), "received a file");
            stream.send(&FileReply::Stored).await?;
            stream.flush().await
        }
        Err(error) => {
            warn!(%error, path = %for_log(&path), "a received file was not kept");
            refuse(stream, error).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_gate_starts_shut_so_a_stream_opened_before_the_login_gets_nothing() {
        // A client can open a bulk stream the instant the connection is up,
        // which is before it has said who it is.
        let gate = Gate::closed();
        assert!(!gate.allows(Permission::FILE_READ));
        assert!(!gate.allows(Permission::FILE_WRITE));
        assert!(!gate.allows(Permission::empty() | Permission::VIEW));
    }

    #[test]
    fn a_gate_carries_exactly_what_the_session_published() {
        let gate = Gate::closed();
        gate.open(Permission::VIEW | Permission::FILE_READ);
        assert!(gate.allows(Permission::FILE_READ));
        // Reading the host's files is not writing to them.
        assert!(!gate.allows(Permission::FILE_WRITE));
        assert!(!gate.allows(Permission::FILE_READ | Permission::FILE_WRITE));
    }

    #[test]
    fn an_empty_requirement_is_met_by_a_shut_gate() {
        // `contains(empty)` is true for every set, which is correct: a request
        // needing nothing is gated on being authenticated, not on a permission.
        // Nothing in `FileRequest` requires nothing, and this pins that.
        let gate = Gate::closed();
        assert!(gate.allows(Permission::empty()));
        for request in [
            FileRequest::List(Location::Places),
            FileRequest::Download { path: "/a".into() },
            FileRequest::Upload {
                path: "/a".into(),
                size: 0,
                replace: false,
            },
        ] {
            assert!(
                !request.required_permission().is_empty(),
                "{request:?} would pass a shut gate"
            );
        }
    }

    #[test]
    fn ending_a_session_shuts_the_gate_on_a_transfer_still_running() {
        // A copy that outlived its connection must stop, not finish quietly in
        // the background with permissions nobody holds any more.
        let gate = Gate::closed();
        gate.open(Permission::all());
        let seen_by_a_transfer = gate.clone();
        gate.close();
        assert!(!seen_by_a_transfer.allows(Permission::FILE_READ));
    }

    #[test]
    fn a_gate_ignores_bits_that_are_not_permissions() {
        // The mirror is a `u32`, and a `u32` can hold values `Permission`
        // cannot. Truncating rather than panicking keeps a future version's
        // extra bit from turning into a crash, and it never grants anything.
        let gate = Gate(Arc::new(AtomicU32::new(u32::MAX)));
        assert!(gate.allows(Permission::FILE_READ));
        let empty = Gate(Arc::new(AtomicU32::new(1 << 31)));
        assert!(!empty.allows(Permission::FILE_READ));
    }
}
