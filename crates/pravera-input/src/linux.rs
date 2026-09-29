//! Input injection on Linux.
//!
//! Not built yet. The plan puts it in P3 alongside the rest of the input work,
//! using `/dev/uinput` through the `evdev` crate.
//!
//! It is worth recording why this backend is the *better* one when it lands,
//! rather than a catch-up port of the Windows path. `uinput` registers a
//! virtual device at the kernel's evdev layer, below X11, Wayland and every
//! application, so injected motion is indistinguishable from a real mouse —
//! including `EV_REL` deltas, which reach games reading raw input. The
//! limitation this crate documents for Windows simply does not exist here, and
//! no driver, signing, or elevation is needed beyond write access to
//! `/dev/uinput`.
//!
//! Returning an error rather than a sink that quietly discards events is the
//! same choice made in `pravera-capture`: a remote desktop where clicks vanish
//! looks exactly like one where the network died.

use crate::{InputError, InputSink, Result};

pub(crate) fn sink() -> Result<Box<dyn InputSink>> {
    Err(InputError::Unavailable(
        "Linux input injection (/dev/uinput) is not implemented yet",
    ))
}
