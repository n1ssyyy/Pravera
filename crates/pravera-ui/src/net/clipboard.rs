//! Keeping two machines' clipboards saying the same thing.
//!
//! ## Polled, not pushed
//!
//! The host never announces a clipboard change. It cannot: the control stream
//! is strictly request-response, so an unsolicited message would land between
//! some other request and its answer and every reply after it would be paired
//! with the wrong question. So this side asks, on a timer, and the question is
//! cheap — "anything since sequence *n*?" — because the host answers it from a
//! counter rather than by opening its clipboard.
//!
//! ## The echo loop
//!
//! The obvious implementation copies text back and forth forever. This end
//! pastes to the host, which moves the host's sequence, so the next poll
//! reports a change, so this end copies its own text home, which moves the
//! local sequence, so it gets pushed again.
//!
//! Two things break it. The host answers a write with the sequence that write
//! produced, so this end can say "I have already seen that one". And
//! [`Clipboards::carried`] remembers the text itself, so identical content is
//! never moved twice even if the sequences disagree.
//!
//! ## Connecting changes nothing
//!
//! The first sweep is a baseline: whatever each machine already held stays
//! where it is. Copying *after* connecting flows in both directions, but
//! connecting on its own never replaces something the person had copied
//! earlier and has not used yet. Silently emptying somebody's clipboard is a
//! thing they notice ten minutes later and cannot undo.

use pravera_core::Permission;
use pravera_files::Clipboard;
use pravera_proto::{ClipboardSeq, ClipboardUpdate, ProtocolError};
use tracing::{debug, warn};

/// How many consecutive failures before this gives up for the session.
///
/// A clipboard is a machine-wide lock that other programs take on their own
/// timers, so a single refusal is ordinary and retrying is right. A run of them
/// means something is actually wrong, and a failing call repeated twice a
/// second for the length of a session is a log nobody can read past.
const MAX_STRIKES: u8 = 6;

/// The two clipboards, and what is known about each.
pub struct Clipboards {
    local: Box<dyn Clipboard>,
    read: bool,
    write: bool,
    /// Where the local clipboard had got to when it was last looked at. A
    /// sequence that still matches means nothing has been copied here since.
    local_seen: ClipboardSeq,
    /// The host's sequence as of its last answer, handed back on the next
    /// question so it only has to describe what changed.
    remote_seen: ClipboardSeq,
    /// The text both machines are believed to hold. The second half of the
    /// echo guard: sequences catch a repeat of the same change, this catches
    /// the same *content* arriving by a different route.
    carried: Option<String>,
    /// Whether the baseline sweep has happened. Until it has, the host's
    /// clipboard is recorded rather than applied. See the module docs.
    primed: bool,
    strikes: u8,
    stopped: bool,
}

impl Clipboards {
    /// Start syncing, if this login is allowed to and this machine has a
    /// clipboard Pravera can reach.
    ///
    /// `None` is an ordinary outcome, not a failure: a `viewer` holds neither
    /// permission, and a session with no window station has nothing to sync.
    /// Either way the rest of the session is unaffected.
    pub fn open(permissions: Permission) -> Option<Clipboards> {
        let read = permissions.contains(Permission::CLIPBOARD_READ);
        let write = permissions.contains(Permission::CLIPBOARD_WRITE);
        if !read && !write {
            debug!("this login may not share the clipboard");
            return None;
        }

        match pravera_files::clipboard::open() {
            Ok(local) => Some(Clipboards::with(local, read, write)),
            Err(error) => {
                debug!(%error, "no clipboard on this machine to share");
                None
            }
        }
    }

    /// Build over a given clipboard. Separate from [`Clipboards::open`] so the
    /// rules can be tested without touching the machine's real one.
    fn with(local: Box<dyn Clipboard>, read: bool, write: bool) -> Clipboards {
        // Taken now, so whatever is already on this clipboard is not mistaken
        // for something the person copied during the session and pushed across.
        let local_seen = local.seq().unwrap_or_default();
        Clipboards {
            local,
            read,
            write,
            local_seen,
            remote_seen: ClipboardSeq::default(),
            carried: None,
            primed: false,
            strikes: 0,
            stopped: false,
        }
    }

    /// Text copied on this machine that the host does not have yet.
    ///
    /// Reads the local clipboard only when its sequence says something has
    /// changed, so the common case — nothing copied since last time — costs one
    /// counter read and takes no locks.
    pub fn outgoing(&mut self) -> Option<String> {
        if !self.write || self.stopped {
            return None;
        }

        match self.local.seq() {
            Ok(seq) if seq == self.local_seen => {
                self.strikes = 0;
                return None;
            }
            Ok(_) => self.strikes = 0,
            Err(error) => {
                self.strike("could not tell whether the clipboard had changed", &error);
                return None;
            }
        }

        let (seq, content) = match self.local.read() {
            Ok(read) => {
                self.strikes = 0;
                read
            }
            Err(error) => {
                self.strike("could not read this machine's clipboard", &error);
                return None;
            }
        };
        // Recorded even when there is nothing to send, so an image or a file
        // list is looked at once rather than on every sweep for the rest of
        // the session.
        self.local_seen = seq;

        match content.text() {
            // Already on both. Either it was just carried here from the host,
            // or the same thing was copied twice.
            Some(text) if self.carried.as_deref() == Some(text) => None,
            Some(text) => Some(text.to_owned()),
            None => None,
        }
    }

    /// Record that a push landed, and what the host's clipboard reads now.
    ///
    /// The sequence is the host's *post-write* one. Without it the next poll is
    /// told this very paste is a change worth carrying home.
    pub fn pushed(&mut self, text: String, seq: ClipboardSeq) {
        self.remote_seen = seq;
        self.carried = Some(text);
        // A push proves the host's clipboard is reachable, so whatever it holds
        // now is known rather than assumed.
        self.primed = true;
    }

    /// The sequence to ask the host about, if this login may read its clipboard.
    pub fn poll_from(&self) -> Option<ClipboardSeq> {
        (self.read && !self.stopped).then_some(self.remote_seen)
    }

    /// Apply what the host answered.
    pub fn incoming(&mut self, update: ClipboardUpdate) {
        if let Some(seq) = update.seq() {
            self.remote_seen = seq;
        }

        let first = !self.primed;
        self.primed = true;

        match update {
            ClipboardUpdate::Unchanged => {}
            // The host holds an image or a file list. Nothing to carry, but the
            // text this end believed both machines shared is gone, so a later
            // copy of that same text is a real change rather than an echo.
            ClipboardUpdate::Uncarried { .. } => self.carried = None,
            ClipboardUpdate::Text { text, .. } => {
                if first {
                    // The baseline. Recorded, deliberately not applied: see the
                    // module documentation.
                    self.carried = Some(text);
                    return;
                }
                if self.carried.as_deref() == Some(text.as_str()) {
                    return;
                }
                match self.local.write(&text) {
                    Ok(seq) => {
                        self.strikes = 0;
                        // The local sequence moved because *this* wrote it.
                        // Recording it is what stops the text being pushed
                        // straight back to the host it came from.
                        self.local_seen = seq;
                        self.carried = Some(text);
                    }
                    Err(error) => {
                        self.strike("could not write this machine's clipboard", &error);
                    }
                }
            }
        }
    }

    /// The host refused. Whether it is worth asking again.
    ///
    /// A permission denial will be denied again — the role does not change
    /// mid-session — so that one stops immediately. Anything else may be a
    /// clipboard the host could not open just then, which is ordinary.
    pub fn refused(&mut self, error: ProtocolError) -> bool {
        if error == ProtocolError::PermissionDenied {
            debug!("this login may not share the host's clipboard after all");
            self.stopped = true;
            return false;
        }
        self.strikes = self.strikes.saturating_add(1);
        if self.strikes >= MAX_STRIKES {
            warn!(
                strikes = self.strikes,
                "the host's clipboard kept refusing; not asking again this session"
            );
            self.stopped = true;
        }
        !self.stopped
    }

    /// Whether this has given up.
    pub fn is_stopped(&self) -> bool {
        self.stopped
    }

    fn strike(&mut self, what: &'static str, error: &pravera_core::Error) {
        self.strikes = self.strikes.saturating_add(1);
        if self.strikes >= MAX_STRIKES {
            warn!(%error, "{what}; not trying again this session");
            self.stopped = true;
        } else {
            debug!(%error, "{what}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pravera_core::{Error, Result};
    use pravera_files::Content;

    /// A clipboard that lives in a struct, so the rules can be exercised
    /// without taking a machine-wide lock or disturbing what somebody copied.
    #[derive(Default)]
    struct Fake {
        content: Option<String>,
        seq: u64,
        /// Every write that reached it, in order. What the echo-loop tests
        /// actually assert on.
        writes: Vec<String>,
        failing: bool,
    }

    impl Fake {
        /// Somebody else on this machine copied something.
        fn elsewhere(&mut self, text: &str) {
            self.content = Some(text.to_owned());
            self.seq += 1;
        }
    }

    /// Handed to `Clipboards`, which owns its clipboard, while the test keeps
    /// looking at the same one.
    #[derive(Clone, Default)]
    struct Shared(std::sync::Arc<std::sync::Mutex<Fake>>);

    impl Shared {
        fn get(&self) -> std::sync::MutexGuard<'_, Fake> {
            self.0.lock().unwrap()
        }
        fn boxed(&self) -> Box<dyn Clipboard> {
            Box::new(self.clone())
        }
    }

    impl Clipboard for Shared {
        fn seq(&self) -> Result<ClipboardSeq> {
            let fake = self.get();
            if fake.failing {
                return Err(Error::Config("the clipboard is busy".into()));
            }
            Ok(ClipboardSeq(fake.seq))
        }

        fn read(&mut self) -> Result<(ClipboardSeq, Content)> {
            let fake = self.get();
            if fake.failing {
                return Err(Error::Config("the clipboard is busy".into()));
            }
            let content = match &fake.content {
                Some(text) => Content::Text(text.clone()),
                None => Content::Uncarried,
            };
            Ok((ClipboardSeq(fake.seq), content))
        }

        fn write(&mut self, text: &str) -> Result<ClipboardSeq> {
            let mut fake = self.get();
            if fake.failing {
                return Err(Error::Config("the clipboard is busy".into()));
            }
            fake.content = Some(text.to_owned());
            fake.writes.push(text.to_owned());
            fake.seq += 1;
            Ok(ClipboardSeq(fake.seq))
        }
    }

    fn both(local: &Shared) -> Clipboards {
        Clipboards::with(local.boxed(), true, true)
    }

    #[test]
    fn a_role_with_neither_permission_does_not_even_open_a_clipboard() {
        assert!(Clipboards::open(Permission::VIEW | Permission::CONTROL).is_none());
    }

    #[test]
    fn connecting_does_not_replace_what_was_already_copied_here() {
        // The whole reason for the baseline sweep. Somebody who copied an
        // address an hour ago and then connects still has the address.
        let local = Shared::default();
        local.get().elsewhere("the address");
        let mut sync = both(&local);

        sync.incoming(ClipboardUpdate::Text {
            seq: ClipboardSeq(9),
            text: "whatever the host had".into(),
        });

        assert!(local.get().writes.is_empty());
        assert_eq!(local.get().content.as_deref(), Some("the address"));
    }

    #[test]
    fn connecting_does_not_push_what_was_already_copied_here_either() {
        // The same rule from the other side: the sequence taken at construction
        // is what stops an hour-old copy being sent across on the first sweep.
        let local = Shared::default();
        local.get().elsewhere("the address");
        let mut sync = both(&local);
        assert_eq!(sync.outgoing(), None);
    }

    #[test]
    fn text_copied_here_after_connecting_goes_across_once() {
        let local = Shared::default();
        let mut sync = both(&local);

        local.get().elsewhere("copied during the session");
        assert_eq!(
            sync.outgoing().as_deref(),
            Some("copied during the session")
        );

        sync.pushed("copied during the session".into(), ClipboardSeq(4));
        // Nothing has changed here since, so a second sweep sends nothing.
        assert_eq!(sync.outgoing(), None);
    }

    #[test]
    fn a_push_is_not_reported_back_as_a_change_to_carry_home() {
        // The echo loop, from the push side. The host's sequence moved because
        // this end moved it, and the answer to the next poll is a description
        // of this end's own paste.
        let local = Shared::default();
        let mut sync = both(&local);

        local.get().elsewhere("mine");
        let text = sync.outgoing().expect("something to push");
        sync.pushed(text, ClipboardSeq(11));

        assert_eq!(sync.poll_from(), Some(ClipboardSeq(11)));
        sync.incoming(ClipboardUpdate::Text {
            seq: ClipboardSeq(11),
            text: "mine".into(),
        });

        assert!(
            local.get().writes.is_empty(),
            "this end wrote its own text back over itself"
        );
        assert_eq!(sync.outgoing(), None, "and then pushed it again");
    }

    #[test]
    fn text_copied_on_the_host_arrives_here_and_is_not_sent_straight_back() {
        // The echo loop from the other side. Writing locally moves the local
        // sequence, and without recording that, the next sweep treats the
        // host's own text as something new to send it.
        let local = Shared::default();
        let mut sync = both(&local);
        sync.incoming(ClipboardUpdate::Unchanged);

        sync.incoming(ClipboardUpdate::Text {
            seq: ClipboardSeq(3),
            text: "copied over there".into(),
        });
        assert_eq!(local.get().writes, vec!["copied over there".to_string()]);

        assert_eq!(sync.outgoing(), None);
        assert_eq!(local.get().writes.len(), 1);
    }

    #[test]
    fn the_same_text_arriving_twice_is_written_once() {
        let local = Shared::default();
        let mut sync = both(&local);
        sync.incoming(ClipboardUpdate::Unchanged);

        for seq in [3, 4, 5] {
            sync.incoming(ClipboardUpdate::Text {
                seq: ClipboardSeq(seq),
                text: "same thing".into(),
            });
        }
        assert_eq!(local.get().writes, vec!["same thing".to_string()]);
    }

    #[test]
    fn an_image_on_the_host_is_not_an_error_and_still_moves_the_sequence_on() {
        let local = Shared::default();
        let mut sync = both(&local);

        sync.incoming(ClipboardUpdate::Uncarried {
            seq: ClipboardSeq(7),
        });
        assert_eq!(sync.poll_from(), Some(ClipboardSeq(7)));
        assert!(local.get().writes.is_empty());
    }

    #[test]
    fn an_image_copied_here_is_looked_at_once_rather_than_every_sweep() {
        let local = Shared::default();
        let mut sync = both(&local);

        // `Fake` reports no content as `Uncarried`, which is what an image
        // reads as: something is there, and it is not ours to move.
        local.get().content = None;
        local.get().seq += 1;

        assert_eq!(sync.outgoing(), None);
        assert_eq!(sync.outgoing(), None);
    }

    #[test]
    fn a_viewer_who_may_only_read_never_pushes() {
        let local = Shared::default();
        let mut sync = Clipboards::with(local.boxed(), true, false);
        local.get().elsewhere("mine");
        assert_eq!(sync.outgoing(), None);
        assert_eq!(sync.poll_from(), Some(ClipboardSeq::default()));
    }

    #[test]
    fn a_login_who_may_only_write_never_polls() {
        let local = Shared::default();
        let sync = Clipboards::with(local.boxed(), false, true);
        assert_eq!(sync.poll_from(), None);
    }

    #[test]
    fn a_refused_permission_stops_immediately_rather_than_asking_again() {
        // The role does not change mid-session, so a second ask has the same
        // answer. Retrying it twice a second for an hour does not.
        let local = Shared::default();
        let mut sync = both(&local);
        assert!(!sync.refused(ProtocolError::PermissionDenied));
        assert!(sync.is_stopped());
        assert_eq!(sync.poll_from(), None);
        assert_eq!(sync.outgoing(), None);
    }

    #[test]
    fn a_host_clipboard_that_is_busy_once_is_asked_again() {
        // Explorer and Office both take the clipboard on their own timers, so
        // a single refusal is ordinary rather than a reason to stop sharing.
        let local = Shared::default();
        let mut sync = both(&local);
        assert!(sync.refused(ProtocolError::Internal));
        assert!(!sync.is_stopped());
    }

    #[test]
    fn a_host_clipboard_that_never_works_is_given_up_on() {
        let local = Shared::default();
        let mut sync = both(&local);
        for _ in 0..MAX_STRIKES {
            sync.refused(ProtocolError::Internal);
        }
        assert!(sync.is_stopped());
    }

    #[test]
    fn a_local_clipboard_that_never_works_is_given_up_on_too() {
        let local = Shared::default();
        let mut sync = both(&local);
        local.get().failing = true;

        for _ in 0..MAX_STRIKES {
            assert_eq!(sync.outgoing(), None);
        }
        assert!(sync.is_stopped());
    }

    #[test]
    fn one_bad_moment_does_not_count_towards_giving_up() {
        // Strikes are consecutive. A clipboard that works, hiccups, and works
        // again must not accumulate its way to being switched off over a long
        // session.
        let local = Shared::default();
        let mut sync = both(&local);

        for _ in 0..(MAX_STRIKES * 3) {
            local.get().failing = true;
            let _ = sync.outgoing();
            local.get().failing = false;
            let _ = sync.outgoing();
        }
        assert!(!sync.is_stopped());
    }
}
