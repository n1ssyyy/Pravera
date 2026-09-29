//! What can go wrong while capturing a screen.
//!
//! Deliberately coarse. A caller can act on "this display is gone" or "the user
//! refused"; it cannot act on the difference between two HRESULTs, so those
//! collapse into [`CaptureError::Backend`] with the platform's own words kept
//! for the log.

use pravera_core::PixelFormat;

use crate::DisplayId;

pub type Result<T, E = CaptureError> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    /// The machine reported no usable displays. Headless servers, and Windows
    /// sessions that are disconnected rather than merely locked.
    #[error("no displays available")]
    NoDisplays,

    #[error("no display with id {0}")]
    NoSuchDisplay(DisplayId),

    /// The backend cannot deliver the requested layout. Capture backends
    /// produce packed 8-bit colour; converting to NV12 is the encoder's job,
    /// so asking capture for it is a programming error rather than a runtime
    /// condition.
    #[error("{0:?} is not a capture output format")]
    UnsupportedFormat(PixelFormat),

    /// The user declined the screen-share prompt, or policy forbids capture.
    /// Distinct from [`CaptureError::Backend`] because it is the one failure
    /// that is not a fault and should be phrased as a choice in the UI.
    #[error("screen capture was not permitted")]
    PermissionDenied,

    /// No backend exists for this platform or session type.
    #[error("screen capture is unavailable here: {0}")]
    Unavailable(&'static str),

    /// The capture stopped on its own: the display was unplugged, the
    /// resolution changed under us, or the compositor revoked the session.
    #[error("the capture was lost")]
    Lost,

    /// Anything the platform said that we cannot classify. The string goes to
    /// the log and the operator, never to a peer.
    #[error("capture backend: {0}")]
    Backend(String),
}

impl CaptureError {
    pub fn backend(message: impl std::fmt::Display) -> Self {
        CaptureError::Backend(message.to_string())
    }

    /// True when starting again might work. A refused permission prompt is not
    /// transient in this sense: retrying just asks again, which is the caller's
    /// decision to make, not the reconnect loop's.
    pub fn is_transient(&self) -> bool {
        matches!(self, CaptureError::Lost | CaptureError::Backend(_))
    }
}

impl From<CaptureError> for pravera_core::Error {
    fn from(error: CaptureError) -> Self {
        pravera_core::Error::Capture(error.to_string())
    }
}
