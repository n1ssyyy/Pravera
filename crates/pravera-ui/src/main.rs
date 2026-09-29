//! The executable. Everything it does lives in the library beside it.
//!
//! One file, three programs, told apart by how Windows started it: the
//! interface when a person runs it, the service when the service control
//! manager runs it, and the agent when the service starts it in a desktop.
//! That is what makes Pravera a single portable executable — see
//! [`pravera_service`] for the table and for why the service launches a
//! separate process at all.
//!
//! Otherwise deliberately empty of logic. The Windows manifest that asks for
//! elevation is attached to this target with a linker argument, and Cargo
//! passes a bin target's link arguments to its unit-test harness as well — so
//! any test that lived here would be compiled into an executable Windows
//! refuses to start without a UAC prompt, which `cargo test` cannot answer.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() -> iced::Result {
    // Before anything else, and before any window: the service control manager
    // gives a service a few seconds to report in, and starting a GPU renderer
    // first would spend them.
    if pravera_service::is_service_launch() {
        pravera_service::run();
        return Ok(());
    }

    pravera_ui::run()
}
