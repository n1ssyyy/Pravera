//! Browsing and transferring the host's files.
//!
//! Every one of these opens its own QUIC stream and closes it when it is done,
//! so a long copy runs *alongside* the session rather than in front of it. That
//! is also why they take `&Session` rather than `&mut Client`: nothing here
//! touches the control stream, so a transfer and a keystroke can be in flight
//! at the same moment.
//!
//! ## Progress is reported, not estimated
//!
//! [`Progress`] carries bytes moved and bytes promised, and nothing else. No
//! rate, no time remaining — those are guesses, and a progress bar that says
//! "3 seconds left" for a minute is worse than one that says nothing. The
//! interface can compute a rate from successive readings if it wants one, and
//! then it owns the guess.
//!
//! ## Cancelling
//!
//! Drop the future. The stream is reset on the way out, the host stops reading
//! the file, and a partial upload leaves a `.pravera-part` file rather than a
//! truncated one under the real name.

use pravera_files::{Sink, Source};
use pravera_proto::{
    FileError, FileReply, FileRequest, Listing, Location, HASH_BYTES, TRANSFER_CHUNK,
};
use pravera_transport::{BulkStream, Session};
use tracing::debug;

use crate::error::{ClientError, Result};

/// How far a transfer has got.
///
/// `moved` is what has actually crossed; `total` is what the far end promised
/// at the start. They are equal exactly once, at the end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    pub moved: u64,
    pub total: u64,
}

impl Progress {
    /// A fraction from 0 to 1, or `None` for an empty file.
    ///
    /// `None` rather than 1.0: an empty file has no progress to show, and
    /// drawing a full bar for something that never moved is a small lie the
    /// interface should get to decide about.
    pub fn fraction(&self) -> Option<f32> {
        (self.total > 0).then(|| self.moved as f32 / self.total as f32)
    }
}

/// Ask the host what is in a directory.
///
/// Needs `FILE_READ`. Refused for a role without it, which the interface should
/// already know from the permission set and not offer.
pub async fn list(session: &Session, location: Location) -> Result<Listing> {
    let mut stream = session.open_bulk().await?;
    stream.send(&FileRequest::List(location)).await?;

    match stream.recv().await? {
        FileReply::Listing(listing) => Ok(listing),
        FileReply::Refused(error) => Err(ClientError::File(error)),
        other => Err(unexpected(&other, "Listing")),
    }
}

/// Copy a file from the host to `destination` on this machine.
///
/// `watch` is called as bytes land, often enough to drive a progress bar and
/// not so often that drawing it costs more than the transfer.
pub async fn download<F>(
    session: &Session,
    path: &str,
    destination: &str,
    replace: bool,
    mut watch: F,
) -> Result<u64>
where
    F: FnMut(Progress),
{
    let mut stream = session.open_bulk().await?;
    stream
        .send(&FileRequest::Download {
            path: path.to_owned(),
        })
        .await?;

    let total = match stream.recv().await? {
        FileReply::Sending { size, .. } => size,
        FileReply::Refused(error) => return Err(ClientError::File(error)),
        other => return Err(unexpected(&other, "Sending")),
    };

    // Created only after the host has agreed to send. Otherwise a refused
    // download would leave a zero-byte file where the person expected a copy.
    let into = destination.to_owned();
    let mut sink = blocking(move || Sink::create(&into, replace)).await?;

    watch(Progress { moved: 0, total });

    let mut buffer = vec![0u8; TRANSFER_CHUNK];
    let mut moved = 0u64;
    while moved < total {
        // `total` never sizes a buffer: a host claiming a 900 TB file gets a
        // fixed-size read like every other host.
        let want = TRANSFER_CHUNK.min((total - moved) as usize);
        stream.read_exact(&mut buffer[..want]).await?;
        moved += want as u64;

        let piece = buffer[..want].to_vec();
        sink = blocking(move || {
            let mut sink = sink;
            sink.write(&piece)?;
            Ok(sink)
        })
        .await?;

        watch(Progress { moved, total });
    }

    let mut expected = [0u8; HASH_BYTES];
    stream.read_exact(&mut expected).await?;
    blocking(move || sink.finish(&expected)).await?;

    debug!(bytes = total, "a file arrived");
    Ok(total)
}

/// Copy a file from this machine to `destination` on the host.
pub async fn upload<F>(
    session: &Session,
    path: &str,
    destination: &str,
    replace: bool,
    mut watch: F,
) -> Result<u64>
where
    F: FnMut(Progress),
{
    // Opened before asking, so a file this end cannot read is reported here
    // rather than after the host has cleared space for it.
    let source_path = path.to_owned();
    let mut source = blocking(move || Source::open(&source_path)).await?;
    let total = source.size();

    let mut stream = session.open_bulk().await?;
    stream
        .send(&FileRequest::Upload {
            path: destination.to_owned(),
            size: total,
            replace,
        })
        .await?;

    match stream.recv().await? {
        FileReply::Ready => {}
        FileReply::Refused(error) => return Err(ClientError::File(error)),
        other => return Err(unexpected(&other, "Ready")),
    }

    watch(Progress { moved: 0, total });

    let mut moved = 0u64;
    loop {
        let (next, piece) = blocking(move || {
            let mut buffer = vec![0u8; TRANSFER_CHUNK];
            let read = source.next(&mut buffer)?;
            buffer.truncate(read);
            Ok((source, buffer))
        })
        .await?;
        source = next;

        if piece.is_empty() {
            break;
        }
        stream.write_all(&piece).await?;
        moved += piece.len() as u64;
        watch(Progress { moved, total });
    }

    stream.write_all(&source.hash()).await?;
    stream.flush().await?;

    // The host verifies the hash and only then moves the file into place, so
    // this reply — not the last byte going out — is what "the file is there"
    // means.
    match stream.recv().await? {
        FileReply::Stored => {
            debug!(bytes = total, "a file was stored");
            Ok(total)
        }
        FileReply::Refused(error) => Err(ClientError::File(error)),
        other => Err(unexpected(&other, "Stored")),
    }
}

/// Run one blocking file operation off the async workers.
///
/// Reading and writing files are blocking syscalls, and the workers they would
/// block are the ones decoding video. A joined task that panicked is reported
/// as an ordinary write failure rather than propagating the panic: a transfer
/// is not worth taking the interface down for.
async fn blocking<T, F>(work: F) -> Result<T>
where
    F: FnOnce() -> std::result::Result<T, FileError> + Send + 'static,
    T: Send + 'static,
{
    match tokio::task::spawn_blocking(work).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) => Err(ClientError::File(error)),
        Err(error) => {
            debug!(%error, "a file operation did not finish");
            Err(ClientError::File(FileError::Unwritable))
        }
    }
}

/// A host that answered something other than what this stage expected.
///
/// The bulk stream is strictly one request and one reply, so this means a host
/// bug or a peer that is not really Pravera.
fn unexpected(reply: &FileReply, expected: &'static str) -> ClientError {
    ClientError::Unexpected {
        expected,
        got: name(reply),
    }
}

fn name(reply: &FileReply) -> &'static str {
    match reply {
        FileReply::Listing(_) => "Listing",
        FileReply::Sending { .. } => "Sending",
        FileReply::Ready => "Ready",
        FileReply::Stored => "Stored",
        FileReply::Refused(_) => "Refused",
    }
}

/// Cancel a transfer that is being abandoned.
///
/// Resets the stream in both directions so the host stops reading the file
/// rather than working through the rest of it for a receiver that has gone.
pub fn cancel(stream: &mut BulkStream) {
    stream.cancel();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_file_has_no_progress_to_draw() {
        // A full bar for something that never moved is a small lie, and the
        // interface should get to decide whether to tell it.
        assert_eq!(Progress { moved: 0, total: 0 }.fraction(), None);
        assert_eq!(
            Progress {
                moved: 0,
                total: 100
            }
            .fraction(),
            Some(0.0)
        );
        assert_eq!(
            Progress {
                moved: 50,
                total: 100
            }
            .fraction(),
            Some(0.5)
        );
        assert_eq!(
            Progress {
                moved: 100,
                total: 100
            }
            .fraction(),
            Some(1.0)
        );
    }

    #[test]
    fn a_host_answering_the_wrong_stage_is_named_in_the_error() {
        let error = unexpected(&FileReply::Ready, "Sending");
        assert!(matches!(
            error,
            ClientError::Unexpected {
                expected: "Sending",
                got: "Ready"
            }
        ));
    }

    #[test]
    fn a_refusal_keeps_the_reason_the_host_gave() {
        // The interface uses it to decide between "try again" and "pick
        // something else", so collapsing it would cost a real distinction.
        let error = ClientError::File(FileError::Exists);
        assert!(matches!(error, ClientError::File(FileError::Exists)));
        assert!(!FileError::Exists.is_worth_retrying());
    }
}
