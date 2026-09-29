//! Browsing and moving files.
//!
//! ## Nothing here touches the control stream
//!
//! Not the bytes of a file, and not the directory listings either. Every one of
//! them opens its own QUIC stream, which is closed when it is done.
//!
//! For the bytes that is obvious: a 40 GB copy on the control stream would sit
//! in front of every keystroke until it finished. For a listing it is less
//! obvious and still true. A directory can hold [`MAX_ENTRIES`] names of 255
//! characters, which is most of a megabyte — a visible stutter in the pointer,
//! every time somebody clicks a folder, on the one stream that must stay
//! responsive. QUIC streams are independent and cost nothing to open, so files
//! get their own and the session never notices them.
//!
//! ## The bulk conversation
//!
//! One request, one stream, no shared state between them. The request names the
//! path, so the host never has to remember a handle it minted earlier — and a
//! client cannot make it remember one either.
//!
//! ```text
//! list       client → host   FileRequest::List(Location)
//!            host → client   FileReply::Listing(..) | Refused(..)
//!
//! download   client → host   FileRequest::Download { path }
//!            host → client   FileReply::Sending { size } | Refused(..)
//!            host → client   <size bytes>
//!            host → client   <32-byte BLAKE3 of those bytes>
//!
//! upload     client → host   FileRequest::Upload { path, size, replace }
//!            host → client   FileReply::Ready | Refused(..)
//!            client → host   <size bytes>
//!            client → host   <32-byte BLAKE3 of those bytes>
//!            host → client   FileReply::Stored | Refused(Corrupt)
//! ```
//!
//! The length is known before the body starts, so the receiver reads exactly
//! that many bytes and then exactly [`HASH_BYTES`] more. `size` is *never* used
//! to size a buffer — the body is read in fixed [`TRANSFER_CHUNK`] pieces
//! straight to disk — so a peer claiming a 900 TB file allocates nothing.
//!
//! ## The hash is not a checksum for the network
//!
//! QUIC already guarantees the bytes arrive intact. The trailer catches the
//! other thing: a file that changed underneath the sender while it was being
//! read. A half-old, half-new copy is worse than a failed transfer, because
//! nothing about it looks wrong.
//!
//! ## Paths
//!
//! Absolute, always, and checked by [`is_safe_path`] on the way in. This is a
//! remote *desktop*: a role holding `FILE_READ` is meant to reach the whole
//! machine, so there is no sandbox to enforce and pretending otherwise would be
//! security theatre. What the check does enforce is that a path means what it
//! looks like — no `..` to walk somewhere the listing never showed, no relative
//! path resolved against whatever directory the host's service happens to be
//! sitting in, no embedded NUL to truncate it inside a system call.

use pravera_core::Permission;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Ceiling on a path, in bytes.
///
/// Above every real filesystem's own limit, so nothing legitimate is refused,
/// and small enough that a listing of [`MAX_ENTRIES`] cannot approach the
/// control message cap.
pub const MAX_PATH_BYTES: usize = 4096;

/// Ceiling on how many entries one listing carries.
///
/// A directory with more than this is reported [`Listing::truncated`] rather
/// than split across messages. Nobody reads the two-thousand-and-first row of a
/// file picker; the answer to a directory that big is to search it, which is a
/// different feature and honestly labelled as missing.
///
/// Chosen against [`crate::MAX_CONTROL_MESSAGE`], not against taste: at 255
/// bytes a name — the longest every mainstream filesystem allows — this many
/// entries plus their metadata still fits in one frame with room to spare. A
/// larger cap would describe directories the host can read and can never send.
/// There is a test.
pub const MAX_ENTRIES: usize = 2048;

/// How much of a file is read, hashed and written at a time.
///
/// Independent of the network's own framing — this is a disk read size, and
/// QUIC does its own packetisation underneath. Large enough that the syscall
/// overhead disappears, small enough that a transfer's memory cost is a
/// constant rather than a function of the file.
pub const TRANSFER_CHUNK: usize = 64 * 1024;

/// Length of the BLAKE3 trailer that follows a transfer body.
pub const HASH_BYTES: usize = 32;

/// What a directory entry is.
///
/// `Other` covers sockets, devices and anything else that is neither a file to
/// copy nor a directory to walk into. Named rather than hidden: a person
/// looking for something they know is there should see it and be told it
/// cannot be moved, not wonder why it vanished.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EntryKind {
    File,
    Directory,
    Other,
}

impl EntryKind {
    pub const fn is_directory(self) -> bool {
        matches!(self, EntryKind::Directory)
    }

    /// Whether this is something a transfer could carry.
    pub const fn is_transferable(self) -> bool {
        matches!(self, EntryKind::File)
    }
}

/// One row of a directory listing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// What the person reads. Ordinarily a file's last path component; in the
    /// places view, a name for a location — "Downloads", "Local disk (C:)".
    pub name: String,
    /// Where this actually is, when that is not the listing's path joined with
    /// the name. Set for the places view, whose rows are shortcuts rather than
    /// children, and `None` everywhere else.
    ///
    /// The join is the host's to make either way: the separator belongs to the
    /// host's filesystem, and a client on the other platform would guess wrong.
    pub path: Option<String>,
    pub kind: EntryKind,
    /// Zero for a directory: the host does not walk into one to total it up,
    /// because a listing that takes a minute is not a listing.
    pub size: u64,
    /// Seconds since the Unix epoch. `None` where the filesystem did not say,
    /// which is a real answer on some network shares.
    pub modified: Option<i64>,
    pub readonly: bool,
}

/// Where a listing came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Location {
    /// The starting points this host offers — drives, home, desktop,
    /// downloads. A client cannot guess these: it does not know the host's
    /// drive letters, its user name, or whether it is Windows at all.
    Places,
    /// One absolute directory.
    Path(String),
}

/// A directory, as the host sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Listing {
    /// The absolute path this describes, spelled the host's way. `None` for
    /// [`Location::Places`], which is a view rather than a directory.
    pub path: Option<String>,
    /// Where "up" goes. `None` at a drive root or in the places view.
    pub parent: Option<String>,
    /// A label for the places view — "This PC", the user's name. Empty
    /// elsewhere, where the path is its own label.
    pub label: String,
    pub entries: Vec<Entry>,
    /// The directory held more than [`MAX_ENTRIES`]. Shown to the person,
    /// because a list that silently stops is a list that lies.
    pub truncated: bool,
}

impl Listing {
    /// The absolute path of one entry.
    ///
    /// An entry carrying its own [`Entry::path`] is taken at its word; anything
    /// else is the listing's path joined with the name, the way this host spells
    /// paths. Falls back to the name alone in the places view, which cannot
    /// happen for a listing this crate built and is still better than a panic
    /// for one a peer sent.
    pub fn path_of(&self, entry: &Entry) -> String {
        if let Some(path) = &entry.path {
            return path.clone();
        }
        let Some(parent) = &self.path else {
            return entry.name.clone();
        };
        let separator = if parent.contains('\\') { '\\' } else { '/' };
        if parent.ends_with(['\\', '/']) {
            format!("{parent}{}", entry.name)
        } else {
            format!("{parent}{separator}{}", entry.name)
        }
    }
}

/// What one side asks for on a bulk stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileRequest {
    /// What is in here? Requires `FILE_READ`.
    ///
    /// Browsing is reading, so `FILE_WRITE` alone does not grant it: a role
    /// that may drop a file into a known folder is not thereby a role that may
    /// look through everything else on the machine.
    List(Location),
    /// Send me this file.
    Download { path: String },
    /// Take this file.
    ///
    /// `replace` is the person's explicit answer to "there is already one of
    /// those". Defaulting it to true would make an upload capable of destroying
    /// something the sender never saw.
    Upload {
        path: String,
        size: u64,
        replace: bool,
    },
}

impl FileRequest {
    /// The path this names, if it names one. `None` for the places view, which
    /// is a question about the host rather than about a location.
    pub fn path(&self) -> Option<&str> {
        match self {
            FileRequest::List(Location::Places) => None,
            FileRequest::List(Location::Path(path))
            | FileRequest::Download { path }
            | FileRequest::Upload { path, .. } => Some(path),
        }
    }

    /// What the sender must hold for this to be honoured.
    ///
    /// Read and write are separate grants and the direction is named from the
    /// *host's* filesystem: a download reads it, an upload writes it. A role
    /// may hold either without the other.
    pub fn required_permission(&self) -> Permission {
        match self {
            FileRequest::List(_) | FileRequest::Download { .. } => Permission::FILE_READ,
            FileRequest::Upload { .. } => Permission::FILE_WRITE,
        }
    }

    /// Whether this is worth acting on at all.
    pub fn is_well_formed(&self) -> bool {
        self.path().is_none_or(is_safe_path)
    }
}

/// What the other side answers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileReply {
    /// What is in the directory that was asked about.
    Listing(Listing),
    /// Download accepted. `size` bytes and then the hash trailer follow.
    Sending {
        size: u64,
        modified: Option<i64>,
    },
    /// Upload accepted. Send the body.
    Ready,
    /// Upload received, hashed, and matching. The file is on disk.
    Stored,
    Refused(FileError),
}

/// Why a transfer did not happen.
///
/// Coarser than the operating system's own error, and deliberately so — but not
/// as coarse as [`crate::ProtocolError`], because whoever is reading this has
/// already authenticated *and* holds a role that grants filesystem access.
/// Telling them a file is missing rather than unreadable is useful to them and
/// tells an attacker nothing they could not learn by listing the directory they
/// were already allowed to list.
///
/// No variant carries a path or an OS message. The detail is in the host's log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error, Serialize, Deserialize)]
pub enum FileError {
    #[error("no such file")]
    NotFound,
    #[error("permission denied")]
    NotPermitted,
    #[error("a file is already there")]
    Exists,
    #[error("not a file that can be transferred")]
    NotAFile,
    #[error("could not be read")]
    Unreadable,
    #[error("could not be written")]
    Unwritable,
    /// The body did not match its hash. The file changed under the sender
    /// mid-read, and what arrived is half of each version.
    #[error("the file changed while it was being sent")]
    Corrupt,
    #[error("the transfer ended early")]
    Truncated,
    #[error("malformed request")]
    Malformed,
}

impl FileError {
    /// Whether trying the same thing again could plausibly work.
    pub const fn is_worth_retrying(self) -> bool {
        matches!(
            self,
            FileError::Unreadable
                | FileError::Unwritable
                | FileError::Corrupt
                | FileError::Truncated
        )
    }
}

/// Whether a peer-supplied path is one this end will act on.
///
/// Not a sandbox — see the module documentation for why there is none. This
/// rejects paths that would not mean what they appear to mean:
///
/// - **Relative**, which would resolve against the host service's working
///   directory. That directory is an implementation detail neither end agreed
///   on, so a relative path is a request whose target nobody knows.
/// - **Containing `..`**, which walks out of the directory the listing showed.
///   A well-behaved client never needs one, because every path it has came from
///   a listing that was already absolute.
/// - **Containing a NUL**, which some system calls treat as the end of the
///   string — so the path that is checked and the path that is opened would be
///   different strings.
/// - **Empty or absurdly long.**
pub fn is_safe_path(path: &str) -> bool {
    if path.is_empty() || path.len() > MAX_PATH_BYTES {
        return false;
    }
    if path.contains('\0') {
        return false;
    }
    if !is_absolute(path) {
        return false;
    }
    // Split on both separators regardless of platform: a Windows host is
    // perfectly willing to open `C:/x/../y`, and a check that only understood
    // backslashes would wave it through.
    path.split(['\\', '/']).all(|component| component != "..")
}

/// Whether a path names a location without needing a working directory.
///
/// Both spellings are accepted on both platforms, because the path was written
/// by the *host* — a Linux client browsing a Windows host receives `C:\Users`
/// and hands it straight back, and neither end should have to translate.
fn is_absolute(path: &str) -> bool {
    // POSIX, and Windows UNC or rooted.
    if path.starts_with('/') || path.starts_with('\\') {
        return true;
    }
    // A drive letter: exactly one ASCII letter, a colon, then a separator.
    let mut chars = path.chars();
    match (chars.next(), chars.next(), chars.next()) {
        (Some(letter), Some(':'), Some('\\' | '/')) => letter.is_ascii_alphabetic(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip<T>(value: &T) -> T
    where
        T: Serialize + serde::de::DeserializeOwned,
    {
        let bytes = postcard::to_allocvec(value).expect("encode");
        postcard::from_bytes(&bytes).expect("decode")
    }

    #[test]
    fn a_path_that_walks_out_of_the_listing_is_refused() {
        // Every path a well-behaved client holds came from a listing, and every
        // listing is absolute. A `..` is either a bug or somebody trying one.
        assert!(!is_safe_path("C:\\Users\\..\\Windows\\System32"));
        assert!(!is_safe_path("/home/../etc/shadow"));
        // And with the other platform's separator, which the host would still
        // happily open.
        assert!(!is_safe_path("C:/Users/../Windows"));
    }

    #[test]
    fn a_relative_path_is_refused_because_nobody_agreed_what_it_is_relative_to() {
        assert!(!is_safe_path("Documents\\report.pdf"));
        assert!(!is_safe_path("./report.pdf"));
        assert!(!is_safe_path("report.pdf"));
        assert!(!is_safe_path("C:report.pdf"), "a drive-relative path");
    }

    #[test]
    fn an_embedded_nul_is_refused() {
        // Otherwise the string that is checked and the string the OS opens are
        // two different strings.
        assert!(!is_safe_path("C:\\Users\\report.pdf\0.txt"));
    }

    #[test]
    fn nothing_and_far_too_much_are_both_refused() {
        assert!(!is_safe_path(""));
        assert!(is_safe_path(&format!(
            "C:\\{}",
            "x".repeat(MAX_PATH_BYTES - 4)
        )));
        assert!(!is_safe_path(&format!(
            "C:\\{}",
            "x".repeat(MAX_PATH_BYTES)
        )));
    }

    #[test]
    fn both_platforms_spellings_are_accepted_on_either_end() {
        // A Linux client browsing a Windows host hands back what the host said,
        // and it must not have to translate first.
        assert!(is_safe_path("C:\\Users\\kim\\report.pdf"));
        assert!(is_safe_path("/home/kim/report.pdf"));
        assert!(is_safe_path("\\\\server\\share\\report.pdf"));
        assert!(is_safe_path("z:/downloads"));
    }

    #[test]
    fn a_lone_dot_is_not_a_walk_upwards() {
        // `.` resolves to the same directory, so there is nothing to refuse and
        // a check that rejected it would refuse legitimate paths.
        assert!(is_safe_path("C:\\Users\\.\\kim"));
        // But a component that merely starts with two dots is a real name.
        assert!(is_safe_path("/home/kim/..hidden"));
    }

    #[test]
    fn a_request_carrying_an_unsafe_path_is_not_well_formed() {
        assert!(FileRequest::Download {
            path: "C:\\Users\\kim\\a.txt".into()
        }
        .is_well_formed());
        assert!(!FileRequest::Download {
            path: "../a.txt".into()
        }
        .is_well_formed());
        assert!(!FileRequest::Upload {
            path: "a.txt".into(),
            size: 10,
            replace: false,
        }
        .is_well_formed());

        // The places view names no path, so there is none to be unsafe.
        assert!(FileRequest::List(Location::Places).is_well_formed());
        assert!(!FileRequest::List(Location::Path("..".into())).is_well_formed());
    }

    #[test]
    fn reading_and_writing_the_hosts_files_are_separate_grants() {
        // Letting somebody drop a file into a downloads folder is not the same
        // as letting them read the machine, and the reverse is not the same
        // either. A role may hold one without the other.
        assert_eq!(
            FileRequest::List(Location::Places).required_permission(),
            Permission::FILE_READ
        );
        assert_eq!(
            FileRequest::Download { path: "/a".into() }.required_permission(),
            Permission::FILE_READ
        );
        assert_eq!(
            FileRequest::Upload {
                path: "/a".into(),
                size: 0,
                replace: false
            }
            .required_permission(),
            Permission::FILE_WRITE
        );
    }

    #[test]
    fn an_entry_is_joined_the_way_the_host_spells_paths() {
        // The separator belongs to the host's filesystem. A Linux client
        // guessing would produce `C:\Users/kim`, which some Windows APIs accept
        // and some do not.
        let windows = Listing {
            path: Some("C:\\Users".into()),
            parent: Some("C:\\".into()),
            label: String::new(),
            entries: Vec::new(),
            truncated: false,
        };
        let entry = Entry {
            name: "kim".into(),
            path: None,
            kind: EntryKind::Directory,
            size: 0,
            modified: None,
            readonly: false,
        };
        assert_eq!(windows.path_of(&entry), "C:\\Users\\kim");

        let posix = Listing {
            path: Some("/home".into()),
            ..windows.clone()
        };
        assert_eq!(posix.path_of(&entry), "/home/kim");
    }

    #[test]
    fn a_root_does_not_get_a_doubled_separator() {
        let root = Listing {
            path: Some("C:\\".into()),
            parent: None,
            label: String::new(),
            entries: Vec::new(),
            truncated: false,
        };
        let entry = Entry {
            name: "Windows".into(),
            path: None,
            kind: EntryKind::Directory,
            size: 0,
            modified: None,
            readonly: false,
        };
        assert_eq!(root.path_of(&entry), "C:\\Windows");
    }

    #[test]
    fn a_place_is_its_own_path() {
        // In the places view there is no parent to join to, so the entry's name
        // has to be the whole path or nothing would open.
        let places = Listing {
            path: None,
            parent: None,
            label: "This PC".into(),
            entries: Vec::new(),
            truncated: false,
        };
        let entry = Entry {
            name: "Downloads".into(),
            path: Some("C:\\Users\\kim\\Downloads".into()),
            kind: EntryKind::Directory,
            size: 0,
            modified: None,
            readonly: false,
        };
        assert_eq!(places.path_of(&entry), "C:\\Users\\kim\\Downloads");
    }

    #[test]
    fn a_transfer_error_never_describes_the_host() {
        // Same rule as `ProtocolError`: the peer is authenticated, but a path
        // or an OS message in an error is still free reconnaissance.
        for error in [
            FileError::NotFound,
            FileError::NotPermitted,
            FileError::Exists,
            FileError::NotAFile,
            FileError::Unreadable,
            FileError::Unwritable,
            FileError::Corrupt,
            FileError::Truncated,
            FileError::Malformed,
        ] {
            let text = error.to_string();
            assert!(!text.contains('/'), "{text:?}");
            assert!(!text.contains('\\'), "{text:?}");
            assert!(!text.contains(':'), "{text:?}");
        }
    }

    #[test]
    fn a_missing_file_is_not_worth_retrying_but_a_locked_one_is() {
        // What the interface uses to decide between offering "try again" and
        // asking the person to pick something else.
        assert!(!FileError::NotFound.is_worth_retrying());
        assert!(!FileError::NotPermitted.is_worth_retrying());
        assert!(!FileError::Exists.is_worth_retrying());
        assert!(FileError::Unreadable.is_worth_retrying());
        assert!(FileError::Corrupt.is_worth_retrying());
    }

    #[test]
    fn the_bulk_conversation_survives_a_round_trip() {
        // postcard does not transmit field names, so a shape disagreement
        // between the two ends is silent misreading rather than a decode error.
        let request = FileRequest::Upload {
            path: "/home/kim/big.iso".into(),
            size: 40 * 1024 * 1024 * 1024,
            replace: true,
        };
        assert_eq!(round_trip(&request), request);

        for reply in [
            FileReply::Sending {
                size: 12,
                modified: Some(1_700_000_000),
            },
            FileReply::Ready,
            FileReply::Stored,
            FileReply::Refused(FileError::Exists),
        ] {
            assert_eq!(round_trip(&reply), reply);
        }
    }

    #[test]
    fn a_listing_survives_a_round_trip() {
        let listing = Listing {
            path: Some("/home/kim".into()),
            parent: Some("/home".into()),
            label: String::new(),
            entries: vec![Entry {
                name: "report.pdf".into(),
                path: None,
                kind: EntryKind::File,
                size: 4096,
                modified: Some(1_700_000_000),
                readonly: true,
            }],
            truncated: true,
        };
        assert_eq!(round_trip(&listing), listing);
        assert_eq!(round_trip(&Location::Places), Location::Places);
    }

    #[test]
    fn a_full_listing_still_fits_in_one_frame() {
        // `MAX_ENTRIES` and `MAX_PATH_BYTES` are chosen together: a listing that
        // could exceed the framing cap would be a directory the host can read
        // and can never send.
        let entries = (0..MAX_ENTRIES)
            .map(|n| Entry {
                // 255 is the longest name every mainstream filesystem allows.
                name: format!("{n:0>255}"),
                // A places entry carries a whole path on top of its name, and
                // the places view is a fixed handful of rows rather than a
                // directory — so the worst case for size is a full directory,
                // where every row's path is `None`.
                path: None,
                kind: EntryKind::File,
                size: u64::MAX,
                modified: Some(i64::MAX),
                readonly: false,
            })
            .collect();
        let listing = Listing {
            path: Some("x".repeat(MAX_PATH_BYTES)),
            parent: Some("x".repeat(MAX_PATH_BYTES)),
            label: String::new(),
            entries,
            truncated: true,
        };
        // Encoded as the reply, not bare: the wrapper is what actually goes on
        // the wire and it is the wrapper that has to fit.
        let encoded = postcard::to_allocvec(&FileReply::Listing(listing)).expect("encode");
        assert!(
            encoded.len() < crate::MAX_CONTROL_MESSAGE,
            "a full listing is {} bytes, over the {} cap",
            encoded.len(),
            crate::MAX_CONTROL_MESSAGE
        );
    }

    #[test]
    fn only_a_plain_file_can_be_transferred() {
        assert!(EntryKind::File.is_transferable());
        assert!(!EntryKind::Directory.is_transferable());
        // A socket or a device node is shown, so somebody looking for it can
        // see it is there, and refused, because copying it is meaningless.
        assert!(!EntryKind::Other.is_transferable());
    }
}
