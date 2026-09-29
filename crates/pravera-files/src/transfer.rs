//! Moving one file.
//!
//! Two halves, each of which knows about a file and nothing about a network:
//! [`Source`] reads and hashes, [`Sink`] writes and verifies. The stream lives
//! in `pravera-host` and `pravera-client`, which drive these. Keeping the I/O
//! and the protocol apart is what lets both be tested — this module against
//! real files with no connection, the protocol against a connection with no
//! disk.
//!
//! ## A partial file never appears under the real name
//!
//! A [`Sink`] writes to a sibling temporary file and renames it into place only
//! after the hash matches. A transfer that dies halfway — a cancelled copy, a
//! dropped connection, a full disk — leaves a `.pravera-part` file that is
//! obviously incomplete, rather than a truncated file that looks finished and
//! is not.
//!
//! That matters most for the case people actually hit: copying a newer version
//! of a file over an older one. Writing in place would destroy the old copy
//! before knowing whether the new one arrived.
//!
//! ## What the hash is for
//!
//! Not the network. QUIC already guarantees the bytes arrive as they were sent.
//! The hash catches a file that changed underneath the *sender* while it was
//! being read — a log still being written, a database mid-checkpoint. What
//! arrives then is half of one version and half of another, and nothing about
//! it looks wrong. [`FileError::Corrupt`] is that case, and it is why the
//! partial file is deleted rather than kept.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use pravera_proto::{FileError, HASH_BYTES};
use tracing::{debug, warn};

use crate::browse::classify;

/// Suffix on a file that is still arriving.
///
/// Visible on purpose. A person who finds one in their downloads folder should
/// be able to tell at a glance that it is debris from an interrupted copy and
/// delete it, rather than wondering what it is.
const PARTIAL: &str = ".pravera-part";

/// A file being read out.
pub struct Source {
    file: File,
    hasher: blake3::Hasher,
    /// What the file measured when it was opened. The length the far end is
    /// promised, and the length it will read: a file that grows mid-transfer
    /// is sent as it was, and one that shrinks fails its hash.
    size: u64,
    modified: Option<i64>,
    sent: u64,
}

impl Source {
    /// Open a file for sending.
    ///
    /// Refuses anything that is not a plain file. A directory has no bytes to
    /// send; a device or a pipe has no end.
    pub fn open(path: &str) -> Result<Source, FileError> {
        let metadata = fs::metadata(path).map_err(|error| {
            debug!(%error, "could not stat a file for sending");
            classify(&error)
        })?;
        if !metadata.is_file() {
            return Err(FileError::NotAFile);
        }

        let file = File::open(path).map_err(|error| {
            debug!(%error, "could not open a file for sending");
            classify(&error)
        })?;

        Ok(Source {
            file,
            hasher: blake3::Hasher::new(),
            size: metadata.len(),
            modified: epoch_seconds(&metadata),
            sent: 0,
        })
    }

    /// How many bytes the far end is being promised.
    pub fn size(&self) -> u64 {
        self.size
    }

    /// When the file was last written, if the filesystem said.
    pub fn modified(&self) -> Option<i64> {
        self.modified
    }

    /// How much has been read so far.
    pub fn sent(&self) -> u64 {
        self.sent
    }

    /// Read the next piece into `buffer`, hashing it on the way past.
    ///
    /// `Ok(0)` means the promised length has been reached. Never reads beyond
    /// it: a file that grew since it was opened is sent as it was measured,
    /// because the length has already been promised and a body longer than its
    /// header would desynchronise the stream.
    pub fn next(&mut self, buffer: &mut [u8]) -> Result<usize, FileError> {
        let left = self.size - self.sent;
        if left == 0 {
            return Ok(0);
        }
        let want = buffer.len().min(left as usize);

        let read = self.file.read(&mut buffer[..want]).map_err(|error| {
            debug!(%error, "a file stopped being readable partway through");
            classify(&error)
        })?;
        if read == 0 {
            // The file shrank. The length was already promised, so this
            // transfer cannot be completed honestly.
            warn!(
                promised = self.size,
                read = self.sent,
                "a file being sent shrank underneath the read"
            );
            return Err(FileError::Truncated);
        }

        self.hasher.update(&buffer[..read]);
        self.sent += read as u64;
        Ok(read)
    }

    /// The hash of everything read. Send it after the body.
    pub fn hash(&self) -> [u8; HASH_BYTES] {
        *self.hasher.finalize().as_bytes()
    }
}

/// A file being written in.
///
/// Dropping one without calling [`Sink::finish`] removes the partial file. A
/// cancelled transfer should not leave litter behind, and the drop path is the
/// only one that runs for every way a transfer can end — including a panic.
pub struct Sink {
    file: Option<File>,
    partial: PathBuf,
    target: PathBuf,
    replace: bool,
    hasher: blake3::Hasher,
    written: u64,
}

impl Sink {
    /// Create a file to receive into.
    ///
    /// `replace` is the person's explicit answer to "there is already one of
    /// those". Without it an upload could destroy something the sender never
    /// saw, so the default has to be to refuse.
    pub fn create(path: &str, replace: bool) -> Result<Sink, FileError> {
        let target = PathBuf::from(path);
        if !replace && target.exists() {
            return Err(FileError::Exists);
        }
        // Refusing here rather than letting the rename silently swallow it:
        // renaming a file over a directory fails on both platforms, and it
        // fails at the very end, after the whole transfer.
        if target.is_dir() {
            return Err(FileError::NotAFile);
        }

        let partial = partial_path(&target)?;
        let file = File::create(&partial).map_err(|error| {
            debug!(%error, "could not create a file to receive into");
            classify(&error)
        })?;

        Ok(Sink {
            file: Some(file),
            partial,
            target,
            replace,
            hasher: blake3::Hasher::new(),
            written: 0,
        })
    }

    /// How much has arrived so far.
    pub fn written(&self) -> u64 {
        self.written
    }

    /// Write the next piece, hashing it on the way past.
    pub fn write(&mut self, bytes: &[u8]) -> Result<(), FileError> {
        let Some(file) = self.file.as_mut() else {
            return Err(FileError::Unwritable);
        };
        file.write_all(bytes).map_err(|error| {
            debug!(%error, "a file being received stopped being writable");
            // A full disk is the common case here, and it is a write failure
            // rather than anything about the sender.
            let _ = error;
            FileError::Unwritable
        })?;
        self.hasher.update(bytes);
        self.written += bytes.len() as u64;
        Ok(())
    }

    /// Check the hash and move the file into place.
    ///
    /// The partial file is removed on any failure, by [`Sink`]'s own drop. A
    /// mismatch means what arrived is not what was sent, and keeping it under
    /// a name that looks finished is the one outcome worse than failing.
    pub fn finish(mut self, expected: &[u8; HASH_BYTES]) -> Result<(), FileError> {
        // Dropped before the rename so every byte is on disk and, on Windows,
        // so nothing still holds a handle to the file being moved.
        let Some(mut file) = self.file.take() else {
            return Err(FileError::Unwritable);
        };
        if let Err(error) = file.flush() {
            debug!(%error, "a received file could not be flushed");
            return Err(FileError::Unwritable);
        }
        drop(file);

        let actual = self.hasher.finalize();
        if actual.as_bytes() != expected {
            warn!(
                bytes = self.written,
                "a received file did not match its hash; it changed while it was being sent"
            );
            return Err(FileError::Corrupt);
        }

        // Checked again rather than trusting the check at creation time. The
        // gap between them is a whole transfer, and `fs::rename` overwrites on
        // both platforms — so without this, a file created during a long upload
        // would be destroyed by it.
        if !self.replace && self.target.exists() {
            return Err(FileError::Exists);
        }

        fs::rename(&self.partial, &self.target).map_err(|error| {
            warn!(%error, "a received file could not be moved into place");
            classify(&error)
        })?;

        // Renamed, so there is nothing left to clean up.
        self.partial = PathBuf::new();
        Ok(())
    }
}

impl Drop for Sink {
    fn drop(&mut self) {
        // Empty once `finish` has renamed it. Every other way out of a
        // transfer — cancelled, disconnected, refused, panicked — arrives here
        // with a partial file to remove.
        if self.partial.as_os_str().is_empty() {
            return;
        }
        self.file.take();
        if let Err(error) = fs::remove_file(&self.partial) {
            debug!(%error, "an interrupted transfer left a partial file behind");
        }
    }
}

/// Where the incomplete copy lives while it is arriving.
///
/// A sibling of the target, so the rename at the end stays on one filesystem —
/// a temporary directory elsewhere would turn it into a second copy of the
/// whole file.
fn partial_path(target: &Path) -> Result<PathBuf, FileError> {
    let name = target.file_name().ok_or(FileError::Malformed)?;
    let parent = target.parent().ok_or(FileError::Malformed)?;
    let mut partial = name.to_os_string();
    partial.push(PARTIAL);
    Ok(parent.join(partial))
}

fn epoch_seconds(metadata: &fs::Metadata) -> Option<i64> {
    let modified = metadata.modified().ok()?;
    match modified.duration_since(std::time::UNIX_EPOCH) {
        Ok(since) => i64::try_from(since.as_secs()).ok(),
        Err(before) => i64::try_from(before.duration().as_secs())
            .ok()
            .map(|seconds| -seconds),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pravera_proto::TRANSFER_CHUNK;

    /// A directory that removes itself, so a failing test does not leave files
    /// in somebody's temp folder for the rest of the year.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Scratch {
            let path = std::env::temp_dir().join(format!("pravera-transfer-{name}"));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("a scratch directory");
            Scratch(path)
        }
        fn at(&self, name: &str) -> String {
            self.0.join(name).to_string_lossy().into_owned()
        }
        fn write(&self, name: &str, bytes: &[u8]) -> String {
            let path = self.at(name);
            fs::write(&path, bytes).expect("write");
            path
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Drive a whole transfer through memory, the way the streams do.
    fn carry(from: &str, to: &str, replace: bool) -> Result<Vec<u8>, FileError> {
        let mut source = Source::open(from)?;
        let mut sink = Sink::create(to, replace)?;
        let mut buffer = vec![0u8; TRANSFER_CHUNK];
        let mut moved = Vec::new();
        loop {
            let read = source.next(&mut buffer)?;
            if read == 0 {
                break;
            }
            moved.extend_from_slice(&buffer[..read]);
            sink.write(&buffer[..read])?;
        }
        sink.finish(&source.hash())?;
        Ok(moved)
    }

    #[test]
    fn a_file_arrives_byte_for_byte() {
        let scratch = Scratch::new("round-trip");
        // Larger than one chunk, and not a multiple of it, so the loop's last
        // partial read is exercised rather than assumed.
        let original: Vec<u8> = (0..TRANSFER_CHUNK * 2 + 37)
            .map(|n| (n % 251) as u8)
            .collect();
        let from = scratch.write("original.bin", &original);
        let to = scratch.at("copy.bin");

        let moved = carry(&from, &to, false).expect("the transfer");
        assert_eq!(moved, original);
        assert_eq!(fs::read(&to).expect("the copy"), original);
    }

    #[test]
    fn an_empty_file_is_a_transfer_rather_than_an_error() {
        let scratch = Scratch::new("empty");
        let from = scratch.write("empty.bin", b"");
        let to = scratch.at("copy.bin");
        assert_eq!(carry(&from, &to, false), Ok(Vec::new()));
        assert!(Path::new(&to).is_file());
    }

    #[test]
    fn a_file_that_is_already_there_is_not_replaced_without_being_asked() {
        // An upload that silently destroys something the sender never saw is
        // the one failure mode with no undo.
        let scratch = Scratch::new("exists");
        let from = scratch.write("new.txt", b"new");
        let to = scratch.write("old.txt", b"old");

        assert_eq!(carry(&from, &to, false), Err(FileError::Exists));
        assert_eq!(fs::read(&to).expect("still there"), b"old");

        carry(&from, &to, true).expect("replacing when asked");
        assert_eq!(fs::read(&to).expect("replaced"), b"new");
    }

    #[test]
    fn a_body_that_does_not_match_its_hash_never_gets_the_real_name() {
        // What a file changing mid-read looks like from this end: the bytes
        // arrived fine, and they are not the bytes that were promised.
        let scratch = Scratch::new("corrupt");
        let to = scratch.at("arriving.bin");

        let mut sink = Sink::create(&to, false).expect("create");
        sink.write(b"what actually arrived").expect("write");
        let wrong = *blake3::hash(b"what was promised").as_bytes();

        assert_eq!(sink.finish(&wrong), Err(FileError::Corrupt));
        assert!(
            !Path::new(&to).exists(),
            "a corrupt file kept the real name"
        );
    }

    #[test]
    fn an_interrupted_transfer_leaves_no_partial_file_behind() {
        let scratch = Scratch::new("interrupted");
        let to = scratch.at("arriving.bin");

        {
            let mut sink = Sink::create(&to, false).expect("create");
            sink.write(b"half of it").expect("write");
            // Dropped without finishing, which is what a cancelled transfer or
            // a dropped connection looks like.
        }

        assert!(!Path::new(&to).exists());
        let leftovers: Vec<_> = fs::read_dir(&scratch.0)
            .expect("read the scratch directory")
            .filter_map(|row| row.ok())
            .map(|row| row.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(leftovers.is_empty(), "left behind: {leftovers:?}");
    }

    #[test]
    fn an_existing_file_survives_an_interrupted_transfer_over_it() {
        // The case people actually hit: copying a newer version over an older
        // one and losing the connection halfway. Writing in place would have
        // destroyed the old copy before knowing the new one would arrive.
        let scratch = Scratch::new("survives");
        let to = scratch.write("important.txt", b"the version that works");

        {
            let mut sink = Sink::create(&to, true).expect("create");
            sink.write(b"the version that do").expect("write");
        }

        assert_eq!(
            fs::read(&to).expect("still there"),
            b"the version that works"
        );
    }

    #[test]
    fn a_directory_is_not_a_file_to_send_or_to_receive_into() {
        let scratch = Scratch::new("directory");
        assert_eq!(
            Source::open(&scratch.0.to_string_lossy()).err(),
            Some(FileError::NotAFile)
        );
        assert_eq!(
            Sink::create(&scratch.0.to_string_lossy(), true).err(),
            Some(FileError::NotAFile)
        );
    }

    #[test]
    fn a_file_that_is_not_there_says_so_without_naming_it() {
        let scratch = Scratch::new("missing");
        let error = Source::open(&scratch.at("nothing.bin"))
            .err()
            .expect("no such file");
        assert_eq!(error, FileError::NotFound);
        assert!(!error.to_string().contains("nothing.bin"));
    }

    #[test]
    fn progress_is_reported_as_it_goes_rather_than_at_the_end() {
        // What the interface draws. A bar that only moves once is not a bar.
        let scratch = Scratch::new("progress");
        let original = vec![7u8; TRANSFER_CHUNK * 3];
        let from = scratch.write("big.bin", &original);

        let mut source = Source::open(&from).expect("open");
        assert_eq!(source.size(), original.len() as u64);
        assert_eq!(source.sent(), 0);

        let mut buffer = vec![0u8; TRANSFER_CHUNK];
        let mut steps = Vec::new();
        while source.next(&mut buffer).expect("read") > 0 {
            steps.push(source.sent());
        }
        assert!(steps.len() >= 3, "reported {} times", steps.len());
        assert_eq!(steps.last(), Some(&(original.len() as u64)));
    }

    #[test]
    fn a_source_never_reads_past_the_length_it_promised() {
        // A log file that grows during the transfer. The header already told
        // the far end how many bytes to expect, and sending more would leave
        // the extra sitting in the stream where the hash trailer should be.
        let scratch = Scratch::new("growing");
        let from = scratch.write("log.txt", b"first");

        let mut source = Source::open(&from).expect("open");
        assert_eq!(source.size(), 5);
        fs::write(&from, b"first and then a great deal more").expect("grow it");

        let mut buffer = vec![0u8; TRANSFER_CHUNK];
        assert_eq!(source.next(&mut buffer).expect("read"), 5);
        assert_eq!(source.next(&mut buffer).expect("read"), 0);
        assert_eq!(source.hash(), *blake3::hash(b"first").as_bytes());
    }

    #[test]
    fn the_partial_file_is_a_sibling_so_the_rename_stays_on_one_filesystem() {
        // A temporary directory elsewhere would make every transfer a copy of
        // the whole file at the moment it completes.
        let partial = partial_path(Path::new("/srv/data/report.pdf")).expect("a partial name");
        assert_eq!(partial.parent(), Path::new("/srv/data/report.pdf").parent());
        assert!(partial
            .file_name()
            .expect("a name")
            .to_string_lossy()
            .ends_with(PARTIAL));
    }
}
