//! JSON-RPC 2.0, newline-delimited, over a loopback-only TCP listener.
//!
//! See the crate documentation for why the transport is a line protocol
//! rather than streamable HTTP. What matters here: one message per line, one
//! reply per request, notifications never answered, and the listener bound to
//! 127.0.0.1 with no way to ask for anything else. An agent on this machine
//! is the client; anything that wants to reach Pravera from elsewhere must go
//! through something else entirely.
//!
//! ## Tool calls never run on the reactor
//!
//! A handler may take tens of milliseconds (a discovery pass) or touch the
//! platform (an injection). Each request is therefore dispatched onto a
//! blocking thread before it reaches the registry, so one slow tool delays
//! its own caller and nobody else's connection.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

use crate::registry::Registry;

/// Where the listener sits unless the UI asks otherwise. Unassigned port
/// space, chosen once; changing it strands every configured agent launch.
pub const DEFAULT_PORT: u16 = 47615;

/// The MCP protocol revision this server speaks. Answered verbatim to
/// `initialize`, so an agent knows what to expect before calling anything.
pub const PROTOCOL_VERSION: &str = "2025-06-18";

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;

/// Why the server failed to come up.
#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    #[error("the MCP listener could not bind {addr}: {source}")]
    Bind {
        addr: SocketAddr,
        #[source]
        source: std::io::Error,
    },
}

/// Serves whatever the attached [`Registry`] holds.
///
/// Constructing one touches no sockets; nothing listens until [`start`](McpServer::start).
/// The UI owns that toggle.
pub struct McpServer {
    registry: Registry,
}

impl McpServer {
    pub fn new(registry: Registry) -> McpServer {
        McpServer { registry }
    }

    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    /// Binds the loopback listener and serves connections until dropped.
    ///
    /// Port `0` asks the OS for a free port, which is what tests do; the
    /// actual address comes back on [`RunningServer::local_addr`] either way.
    /// Multiple sequential clients work for as long as this task runs — each
    /// connection is served independently and a hangup costs nothing.
    pub async fn start(self, port: u16) -> Result<RunningServer, ServerError> {
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
        let listener = TcpListener::bind(addr)
            .await
            .map_err(|source| ServerError::Bind { addr, source })?;
        let local_addr = listener
            .local_addr()
            .map_err(|source| ServerError::Bind { addr, source })?;

        let server = Arc::new(self);
        let task = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        let server = Arc::clone(&server);
                        tokio::spawn(serve_connection(server, stream));
                    }
                    Err(error) => {
                        tracing::warn!(%error, "MCP listener failed to accept");
                        // Transient accept failures should not spin the task
                        // into a hot loop while they persist.
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                }
            }
        });

        Ok(RunningServer { local_addr, task })
    }

    /// Handles one line of input and returns the line to send back, if any.
    ///
    /// Notifications produce `None` — JSON-RPC forbids answering them, and a
    /// client waiting on a reply that never comes would mistake silence for a
    /// dead connection.
    ///
    /// Public because it is the whole protocol in one testable function: the
    /// TCP layer below adds framing and nothing else.
    pub fn respond(&self, line: &str) -> Option<String> {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return None;
        }

        let message: Value = match serde_json::from_str(trimmed) {
            Ok(message) => message,
            Err(error) => {
                return Some(error_reply(Value::Null, PARSE_ERROR, &format!("parse error: {error}")))
            }
        };

        let Some(fields) = message.as_object() else {
            return Some(error_reply(
                Value::Null,
                INVALID_REQUEST,
                "a request must be a JSON object, not an array or a bare value",
            ));
        };
        let id = fields.get("id").cloned().unwrap_or(Value::Null);
        let is_notification = !fields.contains_key("id");

        let Some(method) = fields.get("method").and_then(Value::as_str) else {
            if is_notification {
                return None;
            }
            return Some(error_reply(id, INVALID_REQUEST, "the request has no method"));
        };
        let params = fields.get("params").cloned().unwrap_or_else(|| json!({}));

        let outcome = self.dispatch(method, params);
        if is_notification {
            return None;
        }
        Some(match outcome {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string(),
            Err(failure) => error_reply(id, failure.code, &failure.message),
        })
    }

    fn dispatch(&self, method: &str, params: Value) -> Result<Value, RpcFailure> {
        match method {
            "initialize" => Ok(json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": { "tools": {} },
                "serverInfo": {
                    "name": "pravera",
                    "version": env!("CARGO_PKG_VERSION"),
                },
            })),
            // Recognised so it is not mistaken for a typo; it is a
            // notification, so nothing is ever sent back regardless.
            "notifications/initialized" => Ok(Value::Null),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(self.list_tools()),
            "tools/call" => self.call_tool(params),
            other => Err(RpcFailure {
                code: METHOD_NOT_FOUND,
                message: format!("method `{other}` not found"),
            }),
        }
    }

    /// Enumerates the registry live, so a feature registered moments ago is
    /// in the very next listing without restarting anything.
    fn list_tools(&self) -> Value {
        let tools: Vec<Value> = self
            .registry
            .list()
            .into_iter()
            .map(|spec| {
                json!({
                    "name": spec.name,
                    "description": spec.description,
                    "inputSchema": spec.parameters,
                })
            })
            .collect();
        json!({ "tools": tools })
    }

    fn call_tool(&self, params: Value) -> Result<Value, RpcFailure> {
        let Some(name) = params.get("name").and_then(Value::as_str) else {
            return Err(RpcFailure {
                code: INVALID_PARAMS,
                message: "`tools/call` needs the name of the tool to call".to_string(),
            });
        };
        let arguments = params.get("arguments").cloned().unwrap_or_else(|| json!({}));

        // Every failure the registry reports is a *call* failure, not a
        // protocol failure, and is reported in band as MCP intends: the
        // method was understood and the tool exists or does not, but this
        // invocation went wrong, and an agent that can read why can retry
        // differently.
        let outcome = match self.registry.call(name, arguments) {
            Ok(value) => {
                let text = serde_json::to_string_pretty(&value)
                    .unwrap_or_else(|_| value.to_string());
                json!({ "content": [{ "type": "text", "text": text }], "isError": false })
            }
            Err(error) => {
                json!({ "content": [{ "type": "text", "text": error.to_string() }], "isError": true })
            }
        };
        Ok(outcome)
    }
}

/// A protocol-level refusal, distinct from a tool failing at its job.
struct RpcFailure {
    code: i64,
    message: String,
}

fn error_reply(id: Value, code: i64, message: &str) -> String {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message },
    })
    .to_string()
}

async fn serve_connection(server: Arc<McpServer>, stream: TcpStream) {
    let peer = stream
        .peer_addr()
        .map(|addr| addr.to_string())
        .unwrap_or_else(|_| "unknown".to_string());
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    loop {
        let line = match lines.next_line().await {
            Ok(Some(line)) => line,
            // The agent hung up. An ordinary ending, not a failure.
            Ok(None) => break,
            Err(error) => {
                tracing::debug!(%peer, %error, "MCP connection read failed");
                break;
            }
        };

        let server = Arc::clone(&server);
        let reply = match tokio::task::spawn_blocking(move || server.respond(&line)).await {
            Ok(reply) => reply,
            Err(join_error) => {
                tracing::warn!(%peer, %join_error, "MCP dispatcher task failed");
                break;
            }
        };

        let Some(reply) = reply else { continue };
        // One line, one flush: the agent reads by newline, and batching would
        // hold its next decision open for no reason.
        let framed = format!("{reply}\n");
        if writer.write_all(framed.as_bytes()).await.is_err() || writer.flush().await.is_err() {
            tracing::debug!(%peer, "MCP connection write failed");
            break;
        }
    }

    tracing::debug!(%peer, "MCP connection closed");
}

/// A running listener, and the handle that stops it.
pub struct RunningServer {
    local_addr: SocketAddr,
    task: JoinHandle<()>,
}

impl RunningServer {
    /// The address agents should be pointed at. Always loopback.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Stops serving and waits for the accept loop to finish. Connections in
    /// flight are cut; the registry outlives the server.
    pub async fn stop(self) {
        self.task.abort();
        let _ = self.task.await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use crate::registry::Tool;

    fn request(id: &str, method: &str, params: Value) -> String {
        json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }).to_string()
    }

    fn echo_tool(name: &str) -> Tool {
        Tool::new(name, "test")
            .describes("hands its arguments straight back")
            .handles(Ok)
    }

    async fn started(registry: Registry) -> RunningServer {
        McpServer::new(registry)
            .start(0)
            .await
            .expect("a port-0 bind on loopback cannot collide")
    }

    /// Sends every line down one connection, then collects replies until the
    /// wire goes quiet. Reply order is the order the requests were sent.
    async fn converse(addr: SocketAddr, lines: &[String]) -> Vec<Value> {
        let stream = TcpStream::connect(addr)
            .await
            .expect("the listener we just started must answer");
        let (reader, mut writer) = stream.into_split();
        for line in lines {
            writer
                .write_all(format!("{line}\n").as_bytes())
                .await
                .expect("write to our own listener");
        }
        drop(writer);

        let mut reader = BufReader::new(reader);
        let mut buffer = String::new();
        let mut replies = Vec::new();
        loop {
            match tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut buffer)).await
            {
                Ok(Ok(0)) | Err(_) => break,
                Ok(Ok(_)) => {
                    if !buffer.trim().is_empty() {
                        replies.push(
                            serde_json::from_str(buffer.trim())
                                .expect("this server only sends valid JSON"),
                        );
                    }
                    buffer.clear();
                }
                Ok(Err(error)) => panic!("reading our own listener failed: {error}"),
            }
        }
        replies
    }

    async fn one_reply(addr: SocketAddr, line: String) -> Value {
        let mut replies = converse(addr, &[line]).await;
        assert_eq!(replies.len(), 1, "expected exactly one reply");
        replies.remove(0)
    }

    #[tokio::test]
    async fn an_initialize_handshake_answers_with_version_capabilities_and_server_info() {
        let server = started(Registry::new()).await;
        let reply = one_reply(server.local_addr(), request("h1", "initialize", json!({}))).await;

        assert_eq!(reply["id"], json!("h1"));
        assert_eq!(reply["result"]["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(reply["result"]["capabilities"]["tools"], json!({}));
        assert_eq!(reply["result"]["serverInfo"]["name"], "pravera");
        assert_eq!(reply["result"]["serverInfo"]["version"], env!("CARGO_PKG_VERSION"));
        server.stop().await;
    }

    #[tokio::test]
    async fn an_initialized_notification_is_met_with_silence_not_an_acknowledgement() {
        // The notification goes first, a ping second. If the notification had
        // drawn any reply, two lines would come back and the first would not
        // belong to the ping.
        let server = started(Registry::new()).await;
        let lines = [
            json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }).to_string(),
            request("p1", "ping", json!({})),
        ];
        let replies = converse(server.local_addr(), &lines).await;

        assert_eq!(replies.len(), 1, "a notification drew a reply");
        assert_eq!(replies[0]["id"], json!("p1"));
        server.stop().await;
    }

    #[tokio::test]
    async fn tools_list_reflects_whatever_the_registry_holds_at_the_moment_it_is_asked() {
        // The dynamic-by-construction promise, end to end over the wire.
        let registry = Registry::new();
        registry.register(echo_tool("early_tool")).unwrap();
        let ui_side = registry.clone();
        let server = started(registry).await;
        let addr = server.local_addr();

        let before = one_reply(addr, request("l1", "tools/list", json!({}))).await;
        let names_before: Vec<&str> = before["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t["name"].as_str())
            .collect();
        assert_eq!(names_before, ["early_tool"]);

        // Registered after the server started; no restart, no re-registration
        // with the server, nothing but the registry changing underneath it.
        ui_side.register(echo_tool("late_tool")).unwrap();

        let after = one_reply(addr, request("l2", "tools/list", json!({}))).await;
        let names_after: Vec<&str> = after["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t["name"].as_str())
            .collect();
        assert_eq!(names_after, ["early_tool", "late_tool"]);
        server.stop().await;
    }

    #[tokio::test]
    async fn every_listed_tool_carries_its_description_and_schema() {
        let registry = Registry::new();
        registry.register(echo_tool("described_tool")).unwrap();
        let server = started(registry).await;

        let reply = one_reply(server.local_addr(), request("l", "tools/list", json!({}))).await;
        let tool = &reply["result"]["tools"][0];
        assert_eq!(tool["name"], "described_tool");
        assert_eq!(
            tool["description"],
            "hands its arguments straight back",
            "an agent choosing tools reads this"
        );
        assert_eq!(tool["inputSchema"]["type"], "object");
        server.stop().await;
    }

    #[tokio::test]
    async fn a_tool_call_returns_its_result_as_text_content() {
        let registry = Registry::new();
        registry.register(echo_tool("echo_me")).unwrap();
        let server = started(registry).await;

        let call = request(
            "t1",
            "tools/call",
            json!({ "name": "echo_me", "arguments": { "hello": [1, 2] } }),
        );
        let reply = one_reply(server.local_addr(), call).await;

        assert_eq!(reply["result"]["isError"], false);
        let content = &reply["result"]["content"][0];
        assert_eq!(content["type"], "text");
        let echoed: Value =
            serde_json::from_str(content["text"].as_str().expect("text content")).unwrap();
        assert_eq!(echoed, json!({ "hello": [1, 2] }));
        server.stop().await;
    }

    #[tokio::test]
    async fn a_failing_tool_reports_is_error_true_rather_than_a_protocol_error() {
        let registry = Registry::new();
        registry
            .register(
                Tool::new("honest_failure", "test")
                    .describes("always refuses")
                    .handles(|_| Err("the display fell off".to_string())),
            )
            .unwrap();
        let server = started(registry).await;

        let call = request("t2", "tools/call", json!({ "name": "honest_failure" }));
        let reply = one_reply(server.local_addr(), call).await;

        assert_eq!(reply["result"]["isError"], true);
        assert!(
            reply["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("display fell off"),
            "the agent needs the tool's own words, got {reply}"
        );
        // A tool failing at its job is not a protocol failure, so there is no
        // top-level error object — that distinction is what lets an agent tell
        // "retry differently" apart from "fix your client".
        assert!(reply.get("error").is_none());
        server.stop().await;
    }

    #[tokio::test]
    async fn calling_an_unknown_tool_is_reported_in_band_so_the_agent_can_adjust() {
        let server = started(Registry::new()).await;
        let call = request("t3", "tools/call", json!({ "name": "no_such_tool" }));
        let reply = one_reply(server.local_addr(), call).await;

        assert_eq!(reply["result"]["isError"], true);
        assert!(
            reply["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("no_such_tool"),
            "the message names what was not found"
        );
        server.stop().await;
    }

    #[tokio::test]
    async fn a_tools_call_without_a_name_is_invalid_params() {
        let server = started(Registry::new()).await;
        let reply = one_reply(server.local_addr(), request("t4", "tools/call", json!({}))).await;

        assert_eq!(reply["error"]["code"], -32602);
        server.stop().await;
    }

    #[tokio::test]
    async fn ping_answers_with_an_empty_result_and_no_content() {
        let server = started(Registry::new()).await;
        let reply = one_reply(server.local_addr(), request("p1", "ping", json!({}))).await;
        assert_eq!(reply["result"], json!({}));
        server.stop().await;
    }

    #[tokio::test]
    async fn an_unknown_method_is_method_not_found() {
        let server = started(Registry::new()).await;
        let reply = one_reply(
            server.local_addr(),
            request("u1", "resources/list", json!({})),
        )
        .await;
        assert_eq!(reply["error"]["code"], -32601);
        assert_eq!(reply["id"], json!("u1"));
        server.stop().await;
    }

    #[tokio::test]
    async fn unparseable_input_gets_a_parse_error_and_the_connection_carries_on() {
        // The input comes from a machine; whatever arrives next after garbage
        // must still be served on the same connection.
        let server = started(Registry::new()).await;
        let lines = [
            "this is not json".to_string(),
            request("after-garbage", "ping", json!({})),
        ];
        let replies = converse(server.local_addr(), &lines).await;

        assert_eq!(replies.len(), 2, "garbage killed the connection");
        assert_eq!(replies[0]["error"]["code"], -32700);
        assert_eq!(replies[1]["id"], json!("after-garbage"));
        server.stop().await;
    }

    #[tokio::test]
    async fn a_second_connection_works_after_the_first_one_hangs_up() {
        let server = started(Registry::new()).await;
        let addr = server.local_addr();

        let first = TcpStream::connect(addr).await.unwrap();
        drop(first);

        let second_reply = one_reply(addr, request("c2", "ping", json!({}))).await;
        assert_eq!(second_reply["id"], json!("c2"));
        server.stop().await;
    }

    #[tokio::test]
    async fn the_listener_never_binds_anything_but_loopback() {
        let server = started(Registry::new()).await;
        assert!(
            server.local_addr().ip().is_loopback(),
            "{} is reachable from outside this machine",
            server.local_addr()
        );
        server.stop().await;
    }
}
