//! The MCP bus: a loopback-only, opt-in tool registry + TCP listener.
//!
//! ## Why loopback-only
//!
//! An agent with full input, clipboard and file tools is full control of this
//! machine. Binding to 127.0.0.1 means only code already running here — a local
//! CLI, editor extension or shell — can reach the bus. Anything that wants to
//! drive Pravera from another machine must go through the authenticated, relayed
//! Pravera transport instead, which is the path that actually proves who is
//! asking. Exposing the line protocol on 0.0.0.0 would turn an opt-in local bus
//! into a remote-access backdoor with no authentication beyond being on the same
//! network.
//!
//! ## Why opt-in
//!
//! For the same reason nothing listens until the toggle is on. A dynamic
//! registry costs nothing when idle; a listener costs a port and a promise that
//! something on this machine may move the pointer and read the clipboard. The
//! screen says exactly that, and the default is off.
//!
//! ## The transport
//!
//! JSON-RPC 2.0, newline-delimited, one message per line, over a loopback TCP
//! listener bound to 127.0.0.1 only. The MCP spec's streamable HTTP transport
//! needs headers, sessions and SSE to serve remote clients over untrusted
//! networks; none of those problems exist here because the only intended client
//! is an agent running on the same machine talking to localhost. A line protocol
//! is trivial to test with `nc`, survives a crashed client without state to
//! clean up, and adds no dependency.

use std::net::SocketAddr;
use std::sync::Arc;

use pravera_mcp::{McpServer, Registry, RunningServer, ToolSpec, DEFAULT_PORT};

/// Owns the dynamic registry and the running loopback listener, if any.
///
/// `pravera-mcp` is the source of truth for what tools exist: features call
/// `Registry::global().register(...)` and `tools/list` serves whatever has
/// accumulated. This glue keeps one private [`Registry`] for the UI's listener
/// instead of the global, so tests can seed without polluting each other. The
/// registry is seeded once at startup via [`Mcp::seed`]; thereafter new tools
/// need no MCP-side changes — they register and the next `tools/list` already
/// includes them.
///
/// Nothing listens until [`Mcp::start`] is called. Stopping drops the accept
/// loop; the registry outlives the server.
pub struct Mcp {
    registry: Registry,
    server: Option<RunningServer>,
}

impl Mcp {
    /// A fresh, unseeded bus with no listener.
    pub fn new() -> Mcp {
        Mcp {
            registry: Registry::new(),
            server: None,
        }
    }

    /// The registry this bus serves. Clone is cheap — holders share the same
    /// map — and a late registration reaches holders of older clones.
    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    /// Seed the nine built-in tools. Idempotent: a second call is a no-op
    /// rather than a duplicate-tool error, because the UI may reconstruct the
    /// bus on navigation and should not have to remember whether it already
    /// seeded.
    pub fn seed(&mut self) -> Result<(), String> {
        match pravera_mcp::seed(&self.registry) {
            Ok(()) => Ok(()),
            Err(pravera_mcp::RegistryError::DuplicateTool { .. }) => Ok(()),
            Err(other) => Err(other.to_string()),
        }
    }

    /// Whether the loopback listener is currently accepting connections.
    pub fn is_running(&self) -> bool {
        self.server.is_some()
    }

    /// The loopback address agents should be pointed at, if running.
    /// Always 127.0.0.1 by construction — see module docs.
    pub fn local_addr(&self) -> Option<SocketAddr> {
        self.server.as_ref().map(|s| s.local_addr())
    }

    /// Snapshot of every registered tool, sorted by name.
    pub fn tools(&self) -> Vec<Arc<ToolSpec>> {
        self.registry.list()
    }

    /// Bind the loopback listener and start serving.
    ///
    /// `port == 0` asks the OS for a free port, which is what tests do. An
    /// explicit request for [`DEFAULT_PORT`] falls back to an OS-picked port
    /// when the default is busy — the bus starting somewhere is worth more
    /// than it starting on its favourite number. Any other explicit port that
    /// is busy is a real error: the caller asked for that one, and silently
    /// moving a chosen port would leave an agent pointed at nothing.
    pub async fn start(&mut self, port: u16) -> Result<SocketAddr, String> {
        if self.server.is_some() {
            return Err("the MCP bus is already listening".to_string());
        }
        let server = match McpServer::new(self.registry.clone()).start(port).await {
            Ok(server) => server,
            Err(_) if port == DEFAULT_PORT => {
                // The default is taken; let the OS pick.
                McpServer::new(self.registry.clone())
                    .start(0)
                    .await
                    .map_err(|e| e.to_string())?
            }
            Err(error) => return Err(error.to_string()),
        };
        let addr = server.local_addr();
        debug_assert!(
            addr.ip().is_loopback(),
            "MCP listener bound to {addr}, which is not loopback"
        );
        self.server = Some(server);
        Ok(addr)
    }

    /// Start synchronously by blocking on the current runtime, if any.
    ///
    /// Used from the iced `update` path where spawning a `Task` would require
    /// moving the bus out of `Pravera`. When a tokio handle is available this
    /// parks the other tasks on this worker and blocks safely; otherwise it
    /// returns an error rather than inventing a throwaway runtime whose death
    /// would take the listener with it.
    pub fn start_blocking(&mut self, port: u16) -> Result<SocketAddr, String> {
        // Reuse the async implementation via the current runtime.
        let registry = self.registry.clone();
        let fut = async move {
            match McpServer::new(registry.clone()).start(port).await {
                Ok(server) => Ok(server),
                Err(_) if port == DEFAULT_PORT => {
                    McpServer::new(registry).start(0).await.map_err(|e| e.to_string())
                }
                Err(e) => Err(e.to_string()),
            }
        };

        let server = block_on(fut)??;
        let addr = server.local_addr();
        if self.server.is_some() {
            // Another caller raced us.
            // Drop the just-bound server rather than leaking it.
            // Its task is aborted on drop via `stop` semantics; spawn and forget.
            tokio::spawn(async move { server.stop().await; });
            return Err("the MCP bus is already listening".to_string());
        }
        self.server = Some(server);
        Ok(addr)
    }

    /// Stop serving. In-flight connections are cut; the registry stays.
    pub async fn stop(&mut self) {
        if let Some(server) = self.server.take() {
            server.stop().await;
        }
    }

    /// Stop without awaiting. The accept loop is aborted immediately; the
    /// join handle is cleaned up in the background. Safe to call from a
    /// synchronous `update`.
    pub fn stop_sync(&mut self) {
        if let Some(server) = self.server.take() {
            tokio::spawn(async move { server.stop().await; });
        }
    }
}

impl Default for Mcp {
    fn default() -> Self {
        let mut bus = Mcp::new();
        let _ = bus.seed();
        bus
    }
}

fn block_on<F: std::future::Future>(future: F) -> Result<F::Output, String> {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::CurrentThread => {
            Err("this MCP bus cannot start on a current-thread runtime: its thread cannot be \
                 blocked while it drives the runtime"
                .to_string())
        }
        Ok(handle) => Ok(tokio::task::block_in_place(|| handle.block_on(future))),
        Err(_) => Err("no async runtime available; the MCP bus must be started from a tokio context"
            .to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_bus_is_not_running_and_has_no_address() {
        let bus = Mcp::new();
        assert!(!bus.is_running());
        assert!(bus.local_addr().is_none());
    }

    #[test]
    fn seeding_registers_nine_tools_and_is_idempotent() {
        let mut bus = Mcp::new();
        bus.seed().expect("first seed");
        assert_eq!(bus.tools().len(), 9);
        bus.seed().expect("second seed is a no-op");
        assert_eq!(bus.tools().len(), 9);
    }

    #[test]
    fn tools_come_back_sorted_by_name() {
        let mut bus = Mcp::new();
        bus.seed().unwrap();
        let names: Vec<String> = bus.tools().into_iter().map(|t| t.name.clone()).collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted);
    }

    #[tokio::test]
    async fn the_listener_only_ever_binds_loopback() {
        let mut bus = Mcp::new();
        bus.seed().unwrap();
        let addr = bus.start(0).await.expect("port 0 on loopback cannot collide");
        assert!(addr.ip().is_loopback(), "{addr} is not loopback");
        bus.stop().await;
    }

    #[tokio::test]
    async fn start_twice_is_an_error_and_does_not_leak_a_listener() {
        let mut bus = Mcp::new();
        bus.seed().unwrap();
        bus.start(0).await.unwrap();
        assert!(bus.start(0).await.is_err());
        bus.stop().await;
        assert!(!bus.is_running());
    }

    #[tokio::test]
    async fn start_with_fallback_uses_os_picked_port_when_default_busy() {
        // Occupy DEFAULT_PORT first.
        let mut occupier = Mcp::new();
        occupier.seed().unwrap();
        let occupied = occupier.start(DEFAULT_PORT).await;
        if occupied.is_err() {
            // Port already busy on this machine; skip fallback check rather than flake.
            return;
        }
        let occupied_addr = occupied.unwrap();
        assert_eq!(occupied_addr.port(), DEFAULT_PORT);

        let mut bus = Mcp::new();
        bus.seed().unwrap();
        let addr = bus.start(DEFAULT_PORT).await.expect("fallback to 0");
        assert_ne!(addr.port(), DEFAULT_PORT, "should have fallen back to an ephemeral port");
        assert!(addr.ip().is_loopback());

        bus.stop().await;
        occupier.stop().await;
    }
}
