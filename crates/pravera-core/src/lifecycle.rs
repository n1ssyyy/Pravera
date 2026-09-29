//! The exit-code and port contract between the UI process and the service.
//!
//! One executable is three programs (UI, service, agent — see `main.rs`), and
//! the service keeps an agent alive in the console session. When a freshly
//! launched Pravera finds the single-instance port already held, it has to
//! tell the service *why* it is exiting, because "another instance is here"
//! and "instantly crashed" need opposite reactions: the first means stand
//! down, the second means restart after a pause. Without the distinction the
//! service respawned every five seconds against a running copy, and every
//! respawn could focus-steal the user's window.

/// The exit code a second instance leaves when it exits because the
/// single-instance port is already held — "an instance already covers this
/// session; do not restart me". Never 259 (`STILL_ACTIVE`), which the
/// service uses as its is-alive sentinel.
pub const EXIT_ALREADY_RUNNING: i32 = 7;

/// The port the first instance holds for its lifetime and a second instance
/// probes. A successful connect means a Pravera UI is alive; a refused one
/// means the machine no longer has one and the service should spawn.
pub const SINGLE_INSTANCE_PORT: u16 = 47901;
