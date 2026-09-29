//! The machine's own clipboard.
//!
//! ## Sequence numbers, not polling for content
//!
//! Reading a clipboard is not free: on Windows it takes a global lock that
//! every other program on the machine contends for, and holding it while a
//! remote session polls twice a second is antisocial. So the question asked on
//! the fast path is "has it changed", which Windows answers from a counter
//! without opening anything, and the content is only read when the answer is
//! yes.
//!
//! [`Clipboard::seq`] is that counter. It is opaque: the only operation on it
//! is equality. On Windows it comes from `GetClipboardSequenceNumber`, which
//! moves on every change by any program. Elsewhere it is derived from the
//! content, which costs a read but keeps the same contract.
//!
//! ## Losing a race is not an error
//!
//! Another program can change the clipboard between the sequence check and the
//! read. Nothing here tries to prevent that — the OS offers no way to, and a
//! lock held across a network round trip would be far worse than the race. The
//! read returns whatever was there and the sequence taken *with* it, so the
//! worst case is one extra update.
//!
//! ## What is not carried
//!
//! Text only. An image or a file list reads as [`Content::Uncarried`], which is
//! deliberately not an error: it means "there is something here, and it is not
//! ours to move", and it still moves the sequence so the far end stops asking.

#[cfg(not(windows))]
use pravera_core::Error;
use pravera_core::Result;
use pravera_proto::{ClipboardSeq, MAX_CLIPBOARD_BYTES};

/// What is on the clipboard right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Content {
    Text(String),
    /// Empty, or holding something this version does not move.
    Uncarried,
}

impl Content {
    pub fn text(&self) -> Option<&str> {
        match self {
            Content::Text(text) => Some(text),
            Content::Uncarried => None,
        }
    }
}

/// One machine's clipboard.
pub trait Clipboard: Send {
    /// Where the clipboard has got to, without reading it.
    ///
    /// Cheap enough to call on a timer. See the module documentation for why
    /// that matters.
    fn seq(&self) -> Result<ClipboardSeq>;

    /// What is on the clipboard, and the sequence that goes with *that read*.
    ///
    /// The sequence is taken alongside the content rather than before it, so a
    /// change that lands mid-read is reported as a change rather than being
    /// stamped with a sequence that predates it and then never asked about
    /// again.
    fn read(&mut self) -> Result<(ClipboardSeq, Content)>;

    /// Replace the clipboard with `text`, and report the sequence that produced.
    fn write(&mut self, text: &str) -> Result<ClipboardSeq>;
}

/// This machine's clipboard, if the platform has one Pravera can reach.
pub fn open() -> Result<Box<dyn Clipboard>> {
    #[cfg(windows)]
    {
        Ok(Box::new(windows::WindowsClipboard::new()))
    }
    #[cfg(not(windows))]
    {
        Err(Error::Config(
            "clipboard sharing is not implemented on this platform yet".into(),
        ))
    }
}

/// Whether a string is short enough to send.
///
/// Checked before a read is turned into a message rather than after, so an
/// enormous clipboard costs nothing beyond the read that found it.
pub(crate) fn carriable(text: &str) -> bool {
    !text.is_empty() && text.len() <= MAX_CLIPBOARD_BYTES
}

#[cfg(windows)]
mod windows {
    //! Win32 clipboard access.
    //!
    //! `OpenClipboard` takes a machine-wide lock. Every path here closes it,
    //! including the failure paths: leaving it open makes copy and paste stop
    //! working for every program on the machine until Pravera exits, and the
    //! person has no way to connect that to a remote session.

    use super::{carriable, Clipboard, Content};
    use pravera_core::{Error, Result};
    use pravera_proto::ClipboardSeq;

    use ::windows::Win32::Foundation::{HANDLE, HGLOBAL};
    use ::windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, GetClipboardData, GetClipboardSequenceNumber,
        IsClipboardFormatAvailable, OpenClipboard, SetClipboardData,
    };
    use ::windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
    use ::windows::Win32::System::Ole::CF_UNICODETEXT;

    /// Ceiling on how many UTF-16 code units are read from a clipboard handle.
    ///
    /// The handle is a null-terminated buffer whose length the OS does not
    /// report, so the walk has to stop somewhere. Generous relative to the
    /// protocol's own cap, since text past it is discarded as
    /// [`Content::Uncarried`] rather than truncated.
    const MAX_UNITS: usize = 4 * 1024 * 1024;

    pub(super) struct WindowsClipboard;

    impl WindowsClipboard {
        pub(super) fn new() -> Self {
            WindowsClipboard
        }
    }

    /// Holds the clipboard open, and closes it however the scope is left.
    ///
    /// The `?` operators in the read and write paths are the reason this is a
    /// guard rather than a pair of calls: an early return past a bare
    /// `CloseClipboard` would strand the machine-wide lock.
    struct Opened;

    impl Opened {
        fn new() -> Result<Opened> {
            // Somebody else may hold it for a moment — Explorer and Office both
            // take it on their own timers — so a single refusal is normal.
            unsafe { OpenClipboard(None) }
                .map_err(|error| Error::Config(format!("the clipboard is busy: {error}")))?;
            Ok(Opened)
        }
    }

    impl Drop for Opened {
        fn drop(&mut self) {
            let _ = unsafe { CloseClipboard() };
        }
    }

    fn sequence() -> ClipboardSeq {
        ClipboardSeq(u64::from(unsafe { GetClipboardSequenceNumber() }))
    }

    impl Clipboard for WindowsClipboard {
        fn seq(&self) -> Result<ClipboardSeq> {
            // Deliberately does not open the clipboard. This is the call made on
            // a timer, and taking a machine-wide lock twice a second to learn
            // that nothing happened would slow down every other program.
            Ok(sequence())
        }

        fn read(&mut self) -> Result<(ClipboardSeq, Content)> {
            let _open = Opened::new()?;
            // Taken inside the lock and after the availability check, so it
            // describes the content that is about to be read rather than
            // whatever was there when the caller decided to look.
            let seq = sequence();

            if unsafe { IsClipboardFormatAvailable(CF_UNICODETEXT.0.into()) }.is_err() {
                return Ok((seq, Content::Uncarried));
            }

            let handle = unsafe { GetClipboardData(CF_UNICODETEXT.0.into()) }.map_err(|error| {
                Error::Config(format!("the clipboard could not be read: {error}"))
            })?;

            let text = unsafe { read_utf16(handle) };
            match text {
                Some(text) if carriable(&text) => Ok((seq, Content::Text(text))),
                // Empty, or larger than the protocol carries. Either way there
                // is nothing to send, and saying so moves the sequence on so the
                // far end stops asking about this particular change.
                _ => Ok((seq, Content::Uncarried)),
            }
        }

        fn write(&mut self, text: &str) -> Result<ClipboardSeq> {
            let mut units: Vec<u16> = text.encode_utf16().collect();
            units.push(0);
            let bytes = std::mem::size_of_val(units.as_slice());

            let open = Opened::new()?;
            unsafe { EmptyClipboard() }
                .map_err(|error| Error::Config(format!("the clipboard is locked: {error}")))?;

            // GMEM_MOVEABLE because `SetClipboardData` takes ownership of the
            // block and frees it with `GlobalFree`; anything else is a leak or
            // a crash in whichever program pastes next.
            let block = unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes) }
                .map_err(|error| Error::Config(format!("out of memory for a paste: {error}")))?;

            unsafe {
                let target = GlobalLock(block) as *mut u16;
                if target.is_null() {
                    // The block is still ours: nothing has taken ownership yet.
                    let _ = ::windows::Win32::Foundation::GlobalFree(Some(block));
                    return Err(Error::Config("the paste buffer could not be locked".into()));
                }
                std::ptr::copy_nonoverlapping(units.as_ptr(), target, units.len());
                let _ = GlobalUnlock(block);

                if let Err(error) = SetClipboardData(CF_UNICODETEXT.0.into(), Some(HANDLE(block.0)))
                {
                    // Ownership only transfers on success, so this is still
                    // ours to release.
                    let _ = ::windows::Win32::Foundation::GlobalFree(Some(block));
                    return Err(Error::Config(format!(
                        "the clipboard would not take the text: {error}"
                    )));
                }
            }

            // After closing, not before. `CloseClipboard` is what publishes the
            // change, and the sequence taken while it is still open is the one
            // from before the write — which the caller would then hand back as
            // "I have seen up to here", be told the clipboard had changed, and
            // echo its own text into a loop.
            drop(open);
            Ok(sequence())
        }
    }

    /// Read a null-terminated UTF-16 string out of a clipboard handle.
    ///
    /// # Safety
    ///
    /// `handle` must be a live `CF_UNICODETEXT` handle obtained from
    /// `GetClipboardData` while the clipboard is open, and the clipboard must
    /// stay open for the duration of the call.
    unsafe fn read_utf16(handle: HANDLE) -> Option<String> {
        let block = HGLOBAL(handle.0);
        let start = GlobalLock(block) as *const u16;
        if start.is_null() {
            return None;
        }

        let mut length = 0usize;
        while length < MAX_UNITS && *start.add(length) != 0 {
            length += 1;
        }
        let units = std::slice::from_raw_parts(start, length);
        // Lossy rather than strict: an unpaired surrogate on somebody's
        // clipboard is not a reason to stop syncing, and the replacement
        // character is a smaller lie than dropping the paste.
        let text = String::from_utf16_lossy(units);
        let _ = GlobalUnlock(block);
        Some(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The machine has one clipboard and the test harness runs tests side by
    /// side: every test that touches it takes this first, or one test's write
    /// lands between another's two reads.
    #[cfg(windows)]
    static REAL_CLIPBOARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[cfg(windows)]
    fn hold_the_clipboard() -> std::sync::MutexGuard<'static, ()> {
        REAL_CLIPBOARD.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[test]
    fn nothing_is_not_worth_sending() {
        assert!(!carriable(""));
        assert!(carriable("x"));
    }

    #[test]
    fn a_clipboard_larger_than_the_protocol_carries_is_left_alone() {
        assert!(carriable(&"x".repeat(MAX_CLIPBOARD_BYTES)));
        assert!(!carriable(&"x".repeat(MAX_CLIPBOARD_BYTES + 1)));
    }

    #[test]
    fn an_uncarried_clipboard_has_no_text_to_offer() {
        assert_eq!(Content::Uncarried.text(), None);
        assert_eq!(Content::Text("hi".into()).text(), Some("hi"));
    }

    #[cfg(windows)]
    #[test]
    fn a_round_trip_through_the_real_clipboard_returns_what_went_in() {
        // Touches the machine's actual clipboard, which is why it restores what
        // it found: a test that eats somebody's copied text while they are
        // working is a test that will be deleted rather than fixed.
        let _held = hold_the_clipboard();
        let mut clipboard = match open() {
            Ok(clipboard) => clipboard,
            // A session with no window station — a build agent — has no
            // clipboard at all. Nothing to assert, and nothing wrong.
            Err(_) => return,
        };

        let restore = clipboard.read().ok();
        let before = clipboard.seq().expect("sequence");

        let sent = "pravera clipboard test \u{1f600} ünïcödé";
        let Ok(after_write) = clipboard.write(sent) else {
            return;
        };

        let (_, content) = clipboard.read().expect("read back");
        assert_eq!(content, Content::Text(sent.to_string()));
        assert_ne!(
            after_write, before,
            "a write must report a sequence taken after the change was published, \
             or the far end is handed a stale one and told the clipboard changed again"
        );

        if let Some((_, Content::Text(previous))) = restore {
            let _ = clipboard.write(&previous);
        }
    }

    #[cfg(windows)]
    #[test]
    fn asking_where_the_clipboard_has_got_to_does_not_change_it() {
        let _held = hold_the_clipboard();
        let Ok(clipboard) = open() else { return };
        let Ok(first) = clipboard.seq() else { return };
        let Ok(second) = clipboard.seq() else { return };
        assert_eq!(first, second);
    }
}
