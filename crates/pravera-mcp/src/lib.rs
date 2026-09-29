//! An MCP server that lets an AI agent drive everything the Pravera UI can.
//!
//! The server has no opinions of its own. It holds a [`Registry`] of tool
//! specs, and `tools/list` serves exactly what that registry contains at the
//! moment it is asked. A feature anywhere in Pravera makes a tool appear by
//! calling one `register()` — nothing in this crate changes, which is what
//! keeps "what the agent may do" owned by the features rather than by the
//! protocol layer.
//!
//! ## The wire
//!
//! JSON-RPC 2.0, newline-delimited, one message per line, over a loopback TCP
//! listener bound to 127.0.0.1 only. The MCP spec's streamable HTTP transport
//! needs headers, sessions and SSE to serve remote clients over untrusted
//! networks; none of those problems exist here, because the only intended
//! client is an agent running on the same machine talking to localhost. A line
//! protocol is trivial to test with `nc`, survives a crashed client without
//! state to clean up, and adds no dependency. If Pravera ever needs to expose
//! MCP beyond the machine, HTTP arrives as a second transport in front of the
//! same registry — not as a rewrite of this one.
//!
//! ## Nothing listens unless asked
//!
//! Constructing [`McpServer`] touches no sockets. The listener exists between
//! `start()` and `stop()`, and the UI will own that toggle; a machine that
//! never turns it on never exposes a port.

use std::future::Future;

mod registry;
mod server;
mod tools;

pub use registry::{Handler, Parameters, Registry, RegistryError, Tool, ToolSpec};
pub use server::{McpServer, RunningServer, ServerError, DEFAULT_PORT, PROTOCOL_VERSION};
pub use tools::seed;

/// Run a future to completion from inside a synchronous tool handler.
///
/// Tool handlers are plain functions so a feature can register one without
/// knowing anything about executors, but some tools need to await something —
/// a discovery pass, a subprocess. This is the bridge, and where it runs
/// decides how:
///
/// - On a multi-threaded runtime worker, [`block_in_place`] parks the other
///   tasks on this worker and blocks safely.
/// - On any thread with no runtime (a plain test, a sync caller), a small
///   throwaway current-thread runtime runs the future.
///
/// A *current-thread* runtime is refused outright: its single thread cannot be
/// blocked without deadlocking on itself. The MCP server never does this — it
/// dispatches calls on blocking threads — so reaching this branch means a
/// caller embedded the registry somewhere it should not have, and an honest
/// error beats a hung machine.
pub(crate) fn wait_for<F: Future>(future: F) -> Result<F::Output, String> {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::CurrentThread => {
            Err("this tool cannot run on a current-thread runtime: its thread cannot be \
                 blocked while it drives the runtime"
                .to_string())
        }
        Ok(handle) => Ok(tokio::task::block_in_place(|| handle.block_on(future))),
        Err(_) => {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| format!("no async runtime available and none could be built: {e}"))?;
            Ok(runtime.block_on(future))
        }
    }
}
