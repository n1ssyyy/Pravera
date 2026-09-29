//! Linux screen capture.
//!
//! **Not implemented yet.** The Wayland path is PipeWire behind
//! `xdg-desktop-portal`, which is the only way to capture a Wayland session
//! without patching the compositor, and the X11 fallback is XSHM. Both land
//! with the rest of P1; the plan has the detail.
//!
//! Until then a Linux machine can be the *client* end of a session — the whole
//! transport, protocol and UI work there — it just cannot be the host end.
//! [`crate::SyntheticSource`] still runs, so the pipeline can be exercised on
//! Linux CI without a display server at all.

use crate::{CaptureError, CaptureSource, Result};

pub(crate) fn source() -> Result<Box<dyn CaptureSource>> {
    Err(CaptureError::Unavailable(
        "Linux capture (PipeWire and XSHM) is not implemented yet",
    ))
}
