//! Talking to other machines.
//!
//! Two halves that never meet: [`link`] drives a session this machine started,
//! and [`host`] answers sessions other machines start. A single Pravera can do
//! both at once, which is what makes it peer to peer rather than a viewer and
//! a server.

pub mod clipboard;
pub mod grab;
pub mod host;
pub mod keys;
pub mod known;
pub mod link;
pub mod mcp;
pub mod pointer;
pub mod wake;

use std::sync::{Arc, Mutex};

/// A value that travels in a message but cannot be cloned.
///
/// iced messages must be [`Clone`], because widgets store the message they
/// will publish and hand out a copy on every press. A live session and a bound
/// endpoint are the opposite of that: there must be exactly one of each, and
/// duplicating one would mean two decoders on one connection or two records
/// published for one machine.
///
/// So the message carries a handle, cloning the handle costs nothing, and
/// [`Carry::take`] hands the value over once. A second `take` returns `None`,
/// which is the truthful answer rather than a second copy.
pub struct Carry<T>(Arc<Mutex<Option<T>>>);

impl<T> Carry<T> {
    pub fn new(value: T) -> Carry<T> {
        Carry(Arc::new(Mutex::new(Some(value))))
    }

    /// Take the value. `None` once it has already been taken, or if a previous
    /// holder panicked while holding the lock.
    pub fn take(&self) -> Option<T> {
        self.0.lock().ok()?.take()
    }
}

impl<T> Clone for Carry<T> {
    /// Clones the handle, never the value.
    fn clone(&self) -> Self {
        Carry(self.0.clone())
    }
}

impl<T> std::fmt::Debug for Carry<T> {
    /// Says whether the value is still there without touching it. A `Debug`
    /// that printed the contents would print a session's internals into a log.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let held = self.0.lock().map(|slot| slot.is_some()).unwrap_or(false);
        f.debug_tuple("Carry")
            .field(&if held { "held" } else { "taken" })
            .finish()
    }
}

#[cfg(test)]
mod carry_tests {
    use super::Carry;

    #[test]
    fn the_value_can_only_be_taken_once() {
        // Two takes would mean two owners of something there must be one of.
        let carry = Carry::new(String::from("a session"));
        assert_eq!(carry.take().as_deref(), Some("a session"));
        assert_eq!(carry.take(), None);
    }

    #[test]
    fn a_clone_shares_the_one_value_rather_than_copying_it() {
        let carry = Carry::new(7);
        let copy = carry.clone();

        assert_eq!(carry.take(), Some(7));
        assert_eq!(copy.take(), None, "the clone produced a second value");
    }

    #[test]
    fn debug_does_not_print_what_is_being_carried() {
        let carry = Carry::new(String::from("hunter2"));
        let shown = format!("{carry:?}");
        assert!(!shown.contains("hunter2"), "{shown}");
        assert!(shown.contains("held"), "{shown}");
    }
}
