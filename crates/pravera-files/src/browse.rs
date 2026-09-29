//! Looking at what is on a machine.
//!
//! ## There is no sandbox here
//!
//! A role holding `FILE_READ` is meant to reach the whole machine — that is
//! what a remote desktop is. Confining it to some "shared folder" would be
//! security theatre: the same role can already open a file manager through the
//! video stream and drag anything anywhere. The real boundary is the role, and
//! it is enforced before anything in this module is called.
//!
//! What *is* enforced here is that a path means what it looks like.
//! [`pravera_proto::is_safe_path`] has already rejected relative paths, `..`
//! walks and embedded NULs by the time these functions see one.
//!
//! ## Errors say less than the OS did
//!
//! Every function returns a [`FileError`], which names a category and nothing
//! else. The operating system's own message — which carries the path, the
//! process, and sometimes the user name — is logged on the machine that
//! produced it and never sent anywhere.

use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use pravera_proto::{Entry, EntryKind, FileError, Listing, Location, MAX_ENTRIES};
use tracing::debug;

/// Answer one browsing request.
pub fn browse(location: &Location) -> Result<Listing, FileError> {
    match location {
        Location::Places => Ok(places()),
        Location::Path(path) => list(path),
    }
}

/// Where to start.
///
/// A client cannot work these out for itself: it does not know this machine's
/// drive letters, its user name, or whether it is Windows at all. Never fails —
/// a machine with nothing enumerable still gets an empty list rather than an
/// error, because "there is nowhere to start" is a listing, not a fault.
pub fn places() -> Listing {
    let mut entries = Vec::new();

    if let Some(dirs) = directories::UserDirs::new() {
        // Home first, then the three folders people actually keep things in.
        // Each is checked for existence: `UserDirs` reports where a folder
        // would be, not whether anyone made one.
        push_place(&mut entries, "Home", dirs.home_dir());
        if let Some(path) = dirs.desktop_dir() {
            push_place(&mut entries, "Desktop", path);
        }
        if let Some(path) = dirs.document_dir() {
            push_place(&mut entries, "Documents", path);
        }
        if let Some(path) = dirs.download_dir() {
            push_place(&mut entries, "Downloads", path);
        }
    }

    for (label, path) in volumes() {
        push_place(&mut entries, &label, Path::new(&path));
    }

    Listing {
        path: None,
        parent: None,
        label: host_label(),
        entries,
        truncated: false,
    }
}

/// What is in one directory.
pub fn list(path: &str) -> Result<Listing, FileError> {
    let directory = Path::new(path);
    let reader = fs::read_dir(directory).map_err(|error| {
        debug!(%error, "could not open a directory");
        classify(&error)
    })?;

    let mut entries = Vec::new();
    let mut truncated = false;
    for row in reader {
        if entries.len() >= MAX_ENTRIES {
            // Recorded rather than silently stopped. A list that ends without
            // saying so is a list that lies about what is on the machine.
            truncated = true;
            break;
        }
        // One unreadable row does not spoil the directory. A file being deleted
        // while the listing runs is ordinary, and refusing the whole listing
        // over it would make busy directories unbrowsable.
        match row {
            Ok(row) => entries.push(describe(&row)),
            Err(error) => debug!(%error, "skipped an entry that could not be read"),
        }
    }

    sort(&mut entries);

    Ok(Listing {
        path: Some(path.to_owned()),
        parent: parent_of(directory),
        label: String::new(),
        entries,
        truncated,
    })
}

/// Turn one directory row into an entry.
///
/// Metadata failures are not fatal: a file whose metadata cannot be read is
/// still a file that is there, and showing it with unknown size beats hiding it
/// from somebody who came looking for it.
fn describe(row: &fs::DirEntry) -> Entry {
    let name = row.file_name().to_string_lossy().into_owned();

    // Following symlinks, so a linked directory browses through like the thing
    // it points at. A broken link has no target to describe, which is why the
    // fallback reads the link itself rather than giving up.
    let metadata = row.metadata().or_else(|_| row.path().symlink_metadata());

    let Ok(metadata) = metadata else {
        return Entry {
            name,
            path: None,
            kind: EntryKind::Other,
            size: 0,
            modified: None,
            readonly: true,
        };
    };

    let kind = if metadata.is_dir() {
        EntryKind::Directory
    } else if metadata.is_file() {
        EntryKind::File
    } else {
        EntryKind::Other
    };

    Entry {
        name,
        // A child of the directory being listed, so the host joins it on
        // demand rather than carrying a second copy of the parent per row.
        path: None,
        // Zero for a directory. Totalling one up means walking it, and a
        // listing that takes a minute is not a listing.
        size: if kind.is_directory() {
            0
        } else {
            metadata.len()
        },
        modified: epoch_seconds(metadata.modified().ok()),
        readonly: metadata.permissions().readonly(),
        kind,
    }
}

/// Directories first, then by name, ignoring case.
///
/// Not a preference — it is what every file browser on both platforms does, and
/// a listing sorted any other way reads as broken.
fn sort(entries: &mut [Entry]) {
    entries.sort_by(|a, b| {
        b.kind
            .is_directory()
            .cmp(&a.kind.is_directory())
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
}

fn parent_of(directory: &Path) -> Option<String> {
    let parent = directory.parent()?;
    // `Path::parent` of `C:\` is `None`, but of `C:\Users` it is `C:\` — and of
    // a trailing-separator path it is the path without the separator, which
    // would make "up" a no-op that looks like a broken button.
    if parent == directory || parent.as_os_str().is_empty() {
        return None;
    }
    Some(parent.to_string_lossy().into_owned())
}

/// Offer one starting point, if it is really there.
///
/// `UserDirs` reports where a folder *would* be, not whether anyone made one.
/// A Downloads shortcut that opens onto an error is worse than no shortcut.
///
/// The first label for a path wins. On a machine where Documents has not been
/// redirected several of these resolve to the same folder, and the earlier name
/// is the more specific one.
fn push_place(entries: &mut Vec<Entry>, label: &str, path: &Path) {
    if !path.is_dir() {
        return;
    }
    let full = path.to_string_lossy().into_owned();
    if entries
        .iter()
        .any(|existing| existing.path.as_deref() == Some(full.as_str()))
    {
        return;
    }
    entries.push(Entry {
        name: label.to_owned(),
        // A place is a shortcut, not a child of anything, so it carries its own
        // path rather than being joined onto a parent that does not exist.
        path: Some(full),
        kind: EntryKind::Directory,
        size: 0,
        modified: None,
        readonly: false,
    });
}

fn epoch_seconds(time: Option<SystemTime>) -> Option<i64> {
    let time = time?;
    match time.duration_since(UNIX_EPOCH) {
        Ok(since) => i64::try_from(since.as_secs()).ok(),
        // Before 1970. Rare, real on restored archives, and worth reporting
        // correctly rather than as "unknown".
        Err(before) => i64::try_from(before.duration().as_secs())
            .ok()
            .map(|seconds| -seconds),
    }
}

/// Turn an OS error into the category the peer is told.
///
/// Deliberately lossy. The original is logged by the caller, which is the
/// machine that owns the file and the only place the detail belongs.
pub(crate) fn classify(error: &std::io::Error) -> FileError {
    use std::io::ErrorKind as K;
    match error.kind() {
        K::NotFound => FileError::NotFound,
        K::PermissionDenied => FileError::NotPermitted,
        K::AlreadyExists => FileError::Exists,
        K::InvalidInput | K::InvalidData => FileError::Malformed,
        _ => FileError::Unreadable,
    }
}

/// A name for this machine, shown at the top of the places view.
fn host_label() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "This machine".into())
}

#[cfg(windows)]
fn volumes() -> Vec<(String, String)> {
    use ::windows::core::PCWSTR;
    use ::windows::Win32::Storage::FileSystem::{GetDriveTypeW, GetLogicalDrives};
    // Plain `u32` constants rather than a typed enum, and they live under
    // `WindowsProgramming` rather than beside the call that returns them.
    use ::windows::Win32::System::WindowsProgramming::{
        DRIVE_CDROM, DRIVE_FIXED, DRIVE_REMOTE, DRIVE_REMOVABLE,
    };

    // One bitmask, no I/O. Probing `A:\` through `Z:\` with `is_dir` would
    // instead block for seconds on every disconnected network drive.
    let mask = unsafe { GetLogicalDrives() };
    let mut volumes = Vec::new();
    for letter in 0..26u32 {
        if mask & (1 << letter) == 0 {
            continue;
        }
        let letter = char::from(b'A' + letter as u8);
        let root = format!("{letter}:\\");
        let wide: Vec<u16> = root.encode_utf16().chain(std::iter::once(0)).collect();
        let kind = unsafe { GetDriveTypeW(PCWSTR(wide.as_ptr())) };
        let label = match kind {
            DRIVE_FIXED => format!("Local disk ({letter}:)"),
            DRIVE_REMOVABLE => format!("Removable drive ({letter}:)"),
            DRIVE_REMOTE => format!("Network drive ({letter}:)"),
            DRIVE_CDROM => format!("Disc drive ({letter}:)"),
            _ => format!("Drive ({letter}:)"),
        };
        volumes.push((label, root));
    }
    volumes
}

#[cfg(not(windows))]
fn volumes() -> Vec<(String, String)> {
    // No drive letters to enumerate. The filesystem root is the one place that
    // is always there and always worth offering.
    vec![("Filesystem".to_string(), "/".to_string())]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_listing_of_this_crate_finds_its_own_source() {
        let here = env!("CARGO_MANIFEST_DIR");
        let listing = list(&format!("{here}/src")).expect("this directory exists");
        assert!(listing
            .entries
            .iter()
            .any(|entry| entry.name == "browse.rs" && entry.kind == EntryKind::File));
        assert!(!listing.truncated);
        assert_eq!(
            listing.path.as_deref(),
            Some(format!("{here}/src").as_str())
        );
    }

    #[test]
    fn a_missing_directory_says_so_without_naming_it() {
        let error = list("/pravera-nothing-is-here-42").expect_err("no such directory");
        assert_eq!(error, FileError::NotFound);
        assert!(!error.to_string().contains("pravera-nothing"));
    }

    #[test]
    fn a_file_is_not_a_directory_and_saying_so_is_not_a_crash() {
        let here = env!("CARGO_MANIFEST_DIR");
        // Windows reports this as `NotADirectory` on new toolchains and as a
        // raw OS error on old ones; either way it must be a refusal rather
        // than a panic, and must not be `NotFound`.
        let error = list(&format!("{here}/Cargo.toml")).expect_err("not a directory");
        assert_ne!(error, FileError::NotFound);
    }

    #[test]
    fn directories_come_first_and_then_names_ignoring_case() {
        let mut entries = vec![
            entry("zebra.txt", EntryKind::File),
            entry("Apple", EntryKind::Directory),
            entry("apricot.txt", EntryKind::File),
            entry("banana", EntryKind::Directory),
        ];
        sort(&mut entries);
        let order: Vec<&str> = entries.iter().map(|entry| entry.name.as_str()).collect();
        assert_eq!(order, ["Apple", "banana", "apricot.txt", "zebra.txt"]);
    }

    #[test]
    fn up_from_a_root_goes_nowhere_rather_than_to_itself() {
        // A button that appears to work and does nothing is worse than one that
        // is not offered.
        #[cfg(windows)]
        assert_eq!(parent_of(Path::new("C:\\")), None);
        #[cfg(not(windows))]
        assert_eq!(parent_of(Path::new("/")), None);
    }

    #[test]
    fn up_from_a_directory_goes_to_the_one_above_it() {
        #[cfg(windows)]
        assert_eq!(
            parent_of(Path::new("C:\\Users\\kim")).as_deref(),
            Some("C:\\Users")
        );
        #[cfg(not(windows))]
        assert_eq!(parent_of(Path::new("/home/kim")).as_deref(), Some("/home"));
    }

    #[test]
    fn every_place_offered_is_one_that_actually_exists() {
        // `UserDirs` reports where a folder *would* be. Offering somebody a
        // Downloads shortcut that opens onto an error is worse than not
        // offering one.
        let listing = places();
        assert!(listing.path.is_none(), "places is a view, not a directory");
        for entry in &listing.entries {
            let path = entry.path.as_deref().expect("a place carries its path");
            assert!(
                Path::new(path).is_dir(),
                "{path} was offered and is not there"
            );
            assert!(entry.kind.is_directory());
        }
    }

    #[test]
    fn a_place_reads_as_a_name_and_opens_as_a_path() {
        // The row says "Downloads" and opens the folder it points at. A places
        // view that shows raw paths is a list nobody can scan.
        let listing = places();
        for entry in &listing.entries {
            assert_ne!(Some(entry.name.as_str()), entry.path.as_deref());
            assert_eq!(
                listing.path_of(entry).as_str(),
                entry.path.as_deref().unwrap()
            );
        }
    }

    #[test]
    fn a_place_is_never_offered_twice() {
        // On a machine where Documents has not been redirected, several of
        // these resolve to the same folder.
        let listing = places();
        let mut seen = std::collections::HashSet::new();
        for entry in &listing.entries {
            let path = entry.path.clone().expect("a place carries its path");
            assert!(seen.insert(path.clone()), "{path} twice");
        }
    }

    #[test]
    fn browsing_reaches_both_kinds_of_request() {
        assert!(browse(&Location::Places).is_ok());
        let here = env!("CARGO_MANIFEST_DIR");
        assert!(browse(&Location::Path(format!("{here}/src"))).is_ok());
    }

    #[test]
    fn a_time_before_the_epoch_is_reported_rather_than_dropped() {
        let ancient = UNIX_EPOCH - std::time::Duration::from_secs(86_400);
        assert_eq!(epoch_seconds(Some(ancient)), Some(-86_400));
        assert_eq!(epoch_seconds(Some(UNIX_EPOCH)), Some(0));
        assert_eq!(epoch_seconds(None), None);
    }

    fn entry(name: &str, kind: EntryKind) -> Entry {
        Entry {
            name: name.into(),
            path: None,
            kind,
            size: 0,
            modified: None,
            readonly: false,
        }
    }
}
