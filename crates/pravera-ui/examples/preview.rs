//! The whole application, without the demand for administrator.
//!
//! `pravera.exe` carries a manifest asking Windows to elevate it, which it
//! needs in order to inject input into elevated windows and, later, to talk to
//! the service. That manifest also means it cannot be started without somebody
//! answering a consent prompt — fine once a day, useless when the thing being
//! looked at is a hover state and the loop is build, run, look, adjust.
//!
//! The manifest is attached by `build.rs` to the `pravera` binary by name, so
//! this target does not inherit it and starts like any other program.
//!
//! ```text
//! cargo run --example preview
//! ```
//!
//! Everything except elevation behaves identically: same code, same window,
//! same design system. Anything that genuinely needs the privilege — injecting
//! into a UAC prompt, registering the scheduled task — will fail here, and
//! should be checked with the real binary.

fn main() -> iced::Result {
    pravera_ui::run()
}
