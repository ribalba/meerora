//! A minimal MCP client over stdio for model runtimes, which run the agent
//! loop themselves. It speaks the 2025-06-18 handshake (`initialize`), lists
//! the tools and calls them.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::McpBridge;
use crate::process::kill_group;

const PROTOCOL_VERSION: &str = "2025-06-18";
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(20);
const CALL_TIMEOUT: Duration = Duration::from_secs(120);
/// Most pages of `tools/list` that are followed.
const MAX_PAGES: usize = 10;

/// A tool offered by the server.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct McpTool {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

pub(crate) struct McpClient {
    child: Child,
    stdin: ChildStdin,
    messages: Receiver<Value>,
    next_id: u64,
}

impl McpClient {
    /// Starts the server in `dir` and runs the handshake.
    pub fn start(bridge: &McpBridge, dir: &Path) -> Result<Self, String> {
        let mut child = Command::new(&bridge.command)
            .args(&bridge.args)
            .current_dir(dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .map_err(|e| format!("could not start the meerora MCP bridge ({}): {e}", bridge.command.display()))?;
        let stdin = child.stdin.take().ok_or("the MCP bridge has no stdin")?;
        let stdout = child.stdout.take().ok_or("the MCP bridge has no stdout")?;
        let (tx, messages) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                match serde_json::from_str::<Value>(&line) {
                    Ok(v) => {
                        if tx.send(v).is_err() {
                            break;
                        }
                    }
                    Err(_) => tracing::debug!(line, "MCP bridge printed a line that is not JSON"),
                }
            }
        });
        let mut client = Self { child, stdin, messages, next_id: 1 };
        let params = json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": { "name": "meerora-runtime", "version": env!("CARGO_PKG_VERSION") },
        });
        client.request("initialize", params, HANDSHAKE_TIMEOUT)?;
        client.notify("notifications/initialized")?;
        Ok(client)
    }

    /// Process group of the server, for cancelling.
    pub fn pgid(&self) -> u32 {
        self.child.id()
    }

    pub fn list_tools(&mut self) -> Result<Vec<McpTool>, String> {
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let params = match &cursor {
                Some(c) => json!({ "cursor": c }),
                None => json!({}),
            };
            let result = self.request("tools/list", params, HANDSHAKE_TIMEOUT)?;
            for tool in result.get("tools").and_then(Value::as_array).into_iter().flatten() {
                let Some(name) = tool.get("name").and_then(Value::as_str) else { continue };
                tools.push(McpTool {
                    name: name.to_string(),
                    description: tool.get("description").and_then(Value::as_str).unwrap_or("").to_string(),
                    input_schema: tool.get("inputSchema").cloned().unwrap_or_else(|| json!({ "type": "object" })),
                });
            }
            cursor = result.get("nextCursor").and_then(Value::as_str).map(str::to_string);
            if cursor.is_none() {
                break;
            }
        }
        Ok(tools)
    }

    /// Calls a tool. `Ok` and `Err` both carry text for the model.
    /// `request_capability` and the helper's `config_change` wait for the user's decision (D8).
    pub fn call(&mut self, name: &str, arguments: &Value) -> Result<String, String> {
        let timeout = if matches!(name, "request_capability" | "config_change") { crate::DECISION_WAIT } else { CALL_TIMEOUT };
        let result = self.request("tools/call", json!({ "name": name, "arguments": arguments }), timeout)?;
        let text = result
            .get("content")
            .and_then(Value::as_array)
            .map(|items| items.iter().filter_map(|i| i.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join("\n"))
            .unwrap_or_default();
        if result.get("isError").and_then(Value::as_bool).unwrap_or(false) { Err(text) } else { Ok(text) }
    }

    fn send(&mut self, message: &Value) -> Result<(), String> {
        let mut line = message.to_string();
        line.push('\n');
        self.stdin.write_all(line.as_bytes()).and_then(|_| self.stdin.flush()).map_err(|e| format!("the MCP bridge stopped reading: {e}"))
    }

    fn notify(&mut self, method: &str) -> Result<(), String> {
        self.send(&json!({ "jsonrpc": "2.0", "method": method }))
    }

    fn request(&mut self, method: &str, params: Value, timeout: Duration) -> Result<Value, String> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))?;
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let message = match self.messages.recv_timeout(left) {
                Ok(m) => m,
                Err(RecvTimeoutError::Timeout) => return Err(format!("the MCP bridge did not answer {method} in time")),
                Err(RecvTimeoutError::Disconnected) => return Err("the MCP bridge exited".to_string()),
            };
            // A request from the server. Only ping is understood.
            if let (Some(server_id), Some(server_method)) = (message.get("id"), message.get("method").and_then(Value::as_str)) {
                let reply = if server_method == "ping" {
                    json!({ "jsonrpc": "2.0", "id": server_id, "result": {} })
                } else {
                    json!({ "jsonrpc": "2.0", "id": server_id, "error": { "code": -32601, "message": "method not found" } })
                };
                self.send(&reply)?;
                continue;
            }
            if message.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(error) = message.get("error") {
                let text = error.get("message").and_then(Value::as_str).unwrap_or("unknown error");
                return Err(format!("the MCP bridge refused {method}: {text}"));
            }
            return Ok(message.get("result").cloned().unwrap_or(Value::Null));
        }
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        kill_group(self.child.id());
        let _ = self.child.wait();
    }
}
