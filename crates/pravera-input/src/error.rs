//! What can go wrong while replaying an event on this machine.

pub type Result<T, E = InputError> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum InputError {
    /// No injection backend for this platform or session type.
    #[error("input injection is unavailable here: {0}")]
    Unavailable(&'static str),

    /// The event named a key this backend cannot press.
    ///
    /// Not a failure of the session: an unmapped key is one key, and the rest
    /// of the keyboard keeps working. Logged, counted, and otherwise ignored.
    #[error("no mapping for HID usage {0:#06x}")]
    UnmappedKey(u16),

    /// The event itself was not sane: a NaN coordinate, a fraction outside
    /// `0.0..=1.0`, an empty or oversized text run.
    ///
    /// These arrive from the network, so they are refused rather than fixed
    /// up. Clamping a NaN parks the pointer in a corner and reports success,
    /// which is the worst of both outcomes.
    #[error("the event was malformed")]
    Malformed,

    /// The platform refused the injection. On Windows this is almost always
    /// UIPI: a process at medium integrity cannot send input to one running
    /// elevated, which is why the session agent has to run as SYSTEM.
    #[error("the platform refused the injection: {0}")]
    Refused(String),

    #[error("input backend: {0}")]
    Backend(String),
}

impl InputError {
    pub fn backend(message: impl std::fmt::Display) -> Self {
        InputError::Backend(message.to_string())
    }

    /// Whether the rest of the session can carry on after this.
    ///
    /// An unmapped key is a gap in a table; a refused injection means every
    /// later event will be refused too, and the operator needs to be told
    /// rather than left clicking at a screen that ignores them.
    pub fn is_recoverable(&self) -> bool {
        matches!(self, InputError::UnmappedKey(_))
    }
}

impl From<InputError> for pravera_core::Error {
    fn from(error: InputError) -> Self {
        pravera_core::Error::Input(error.to_string())
    }
}
