//! MCP over newline-delimited JSON-RPC 2.0, one message per line.
//!
//! The server is dual-era. Legacy clients open with `initialize` and get one
//! of [`LEGACY_VERSIONS`]. Modern clients (2026-07-28) send the protocol
//! version in the `_meta` of every request and may probe with
//! `server/discover` first, which claude 2.1.299 does. Requests without that
//! `_meta` are served the legacy way, so a client that skips `initialize`
//! still works.

use serde_json::{Map, Value, json};

use crate::daemon::{CallError, Daemon, Reply};
use crate::tools::{CallError as ParseError, Tools};

/// Versions with per-request `_meta` and without `initialize`, newest first.
pub const MODERN_VERSIONS: &[&str] = &["2026-07-28"];
/// Versions negotiated by `initialize`, newest first.
pub const LEGACY_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

const VERSION_KEY: &str = "io.modelcontextprotocol/protocolVersion";
const CAPABILITIES_KEY: &str = "io.modelcontextprotocol/clientCapabilities";
const CLIENT_INFO_KEY: &str = "io.modelcontextprotocol/clientInfo";
const SERVER_INFO_KEY: &str = "io.modelcontextprotocol/serverInfo";

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const UNSUPPORTED_VERSION: i64 = -32022;

/// How long modern clients may cache `server/discover` and `tools/list`.
/// The tools are built once at startup, so they never change in between.
const CACHE_TTL_MS: u64 = 300_000;

const INSTRUCTIONS: &str = "These tools act on your figure in the meerora space, where the user watches you work. \
                            Walk to the station that fits your task, keep your activity line current, post short notes \
                            on progress and call ask_user when you need the user. Call request_capability when the task \
                            needs a capability you do not hold. apps_list, app_read, app_screenshot, app_act and app_open \
                            show you the windows of your view and let you use them, with the capabilities apps.read, \
                            apps.control, apps.input and apps.launch.";

struct RpcError {
    code: i64,
    message: String,
    data: Option<Value>,
}

impl RpcError {
    fn new(code: i64, message: impl Into<String>) -> Self {
        Self { code, message: message.into(), data: None }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Era {
    Legacy,
    Modern,
}

pub struct Server {
    tools: Tools,
    daemon: Daemon,
    /// The modern client is logged once, not on every request.
    logged_modern_client: bool,
}

impl Server {
    pub fn new(tools: Tools, daemon: Daemon) -> Self {
        Self { tools, daemon, logged_modern_client: false }
    }

    /// Handles one line from stdin and returns the line to write to stdout,
    /// without the newline. Notifications and client responses get none.
    pub fn handle_line(&mut self, line: &str) -> Option<String> {
        let line = line.trim();
        if line.is_empty() {
            return None;
        }
        let reply = match serde_json::from_str::<Value>(line) {
            Err(e) => Some(error_response(Value::Null, RpcError::new(PARSE_ERROR, format!("The line is not valid JSON ({e})")))),
            // Batches exist in 2025-03-26 only, but cost nothing to accept.
            Ok(Value::Array(batch)) if batch.is_empty() => Some(error_response(Value::Null, RpcError::new(INVALID_REQUEST, "Empty batch"))),
            Ok(Value::Array(batch)) => {
                let replies: Vec<Value> = batch.into_iter().filter_map(|m| self.handle_message(m)).collect();
                (!replies.is_empty()).then_some(Value::Array(replies))
            }
            Ok(message) => self.handle_message(message),
        };
        reply.map(|r| serde_json::to_string(&r).expect("JSON values always serialize"))
    }

    fn handle_message(&mut self, message: Value) -> Option<Value> {
        let Value::Object(mut message) = message else {
            return Some(error_response(Value::Null, RpcError::new(INVALID_REQUEST, "A message must be a JSON object")));
        };
        let id = message.remove("id");
        let reply_id = match &id {
            Some(id @ (Value::String(_) | Value::Number(_))) => id.clone(),
            _ => Value::Null,
        };
        if message.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
            return Some(error_response(reply_id, RpcError::new(INVALID_REQUEST, "jsonrpc must be \"2.0\"")));
        }
        let method = match message.remove("method") {
            Some(Value::String(method)) => method,
            // A response from the client. This server sends no requests.
            None if message.contains_key("result") || message.contains_key("error") => return None,
            _ => return Some(error_response(reply_id, RpcError::new(INVALID_REQUEST, "method must be a string"))),
        };
        let params = message.remove("params").unwrap_or(Value::Null);
        match id {
            // Notifications never get a reply, known or not, and none needs an
            // action here. notifications/initialized changes nothing, and a
            // call is already answered when its notifications/cancelled is read.
            None => None,
            Some(Value::String(_) | Value::Number(_)) => Some(match self.request(&method, params) {
                Ok(result) => json!({ "jsonrpc": "2.0", "id": reply_id, "result": result }),
                Err(e) => error_response(reply_id, e),
            }),
            Some(_) => Some(error_response(Value::Null, RpcError::new(INVALID_REQUEST, "id must be a string or a number"))),
        }
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value, RpcError> {
        let params = match params {
            Value::Null => Map::new(),
            Value::Object(params) => params,
            _ => return Err(RpcError::new(INVALID_PARAMS, "params must be an object")),
        };
        let era = self.era(method, &params)?;
        let mut result = match method {
            "initialize" => initialize(&params),
            "ping" => json!({}),
            "server/discover" => json!({
                "supportedVersions": MODERN_VERSIONS.iter().chain(LEGACY_VERSIONS).collect::<Vec<_>>(),
                "capabilities": capabilities(),
                "instructions": INSTRUCTIONS,
            }),
            "tools/list" => json!({ "tools": self.tools.definitions() }),
            "tools/call" => self.call(&params)?,
            _ => return Err(RpcError::new(METHOD_NOT_FOUND, format!("Unknown method {method}"))),
        };
        if era == Era::Modern {
            result["resultType"] = json!("complete");
            result["_meta"] = json!({ SERVER_INFO_KEY: server_info() });
            if matches!(method, "server/discover" | "tools/list") {
                result["ttlMs"] = json!(CACHE_TTL_MS);
                result["cacheScope"] = json!("private");
            }
        }
        Ok(result)
    }

    /// Modern requests carry a modern version in `_meta`. `initialize` is
    /// always legacy, and so is everything else without that `_meta`, except
    /// `server/discover`, which only modern clients send.
    fn era(&mut self, method: &str, params: &Map<String, Value>) -> Result<Era, RpcError> {
        if method == "initialize" {
            return Ok(Era::Legacy);
        }
        let meta = params.get("_meta").and_then(Value::as_object);
        let Some(version) = meta.and_then(|m| m.get(VERSION_KEY)) else {
            return Ok(if method == "server/discover" { Era::Modern } else { Era::Legacy });
        };
        let Some(version) = version.as_str() else {
            return Err(RpcError::new(INVALID_PARAMS, format!("_meta.{VERSION_KEY} must be a string")));
        };
        if LEGACY_VERSIONS.contains(&version) {
            return Ok(Era::Legacy);
        }
        if !MODERN_VERSIONS.contains(&version) {
            crate::log!("a client asked for MCP {version}, which this server does not support");
            return Err(RpcError {
                code: UNSUPPORTED_VERSION,
                message: "Unsupported protocol version".into(),
                data: Some(json!({ "supported": MODERN_VERSIONS.iter().chain(LEGACY_VERSIONS).collect::<Vec<_>>(), "requested": version })),
            });
        }
        if !meta.and_then(|m| m.get(CAPABILITIES_KEY)).is_some_and(Value::is_object) {
            return Err(RpcError::new(INVALID_PARAMS, format!("_meta.{CAPABILITIES_KEY} is missing")));
        }
        if !self.logged_modern_client {
            self.logged_modern_client = true;
            let client = meta.and_then(|m| m.get(CLIENT_INFO_KEY)).map(client_name).unwrap_or_else(|| "a client".into());
            crate::log!("{client} speaks MCP {version}");
        }
        Ok(Era::Modern)
    }

    fn call(&mut self, params: &Map<String, Value>) -> Result<Value, RpcError> {
        let Some(name) = params.get("name").and_then(Value::as_str) else {
            return Err(RpcError::new(INVALID_PARAMS, "tools/call needs the name of a tool"));
        };
        let args = match params.get("arguments") {
            None | Some(Value::Null) => Map::new(),
            Some(Value::Object(args)) => args.clone(),
            Some(_) => return Err(RpcError::new(INVALID_PARAMS, "arguments must be an object")),
        };
        let call = match self.tools.parse(name, &args) {
            Ok(call) => call,
            Err(ParseError::UnknownTool) => return Err(RpcError::new(INVALID_PARAMS, format!("Unknown tool {name}"))),
            Err(ParseError::Invalid(message)) => return Ok(tool_result(&message, true)),
        };
        let done = match &call {
            meerora_protocol::ToolCall::AskUser { .. } => "The user sees your question. End your turn now.",
            _ => "Done.",
        };
        let result = match self.daemon.call(call) {
            Ok(Reply { ok: true, message, image }) => {
                with_image(tool_result(if message.is_empty() { done } else { &message }, false), image)
            }
            Ok(Reply { ok: false, message, .. }) => {
                tool_result(if message.is_empty() { "meerora refused the call." } else { &message }, true)
            }
            Err(CallError::Timeout(_)) if name == "request_capability" => {
                let text = "The user did not decide within 10 minutes. Go on without it. If they allow it later, meerora continues \
                            this conversation with it.";
                crate::log!("a capability request got no decision in time");
                tool_result(text, true)
            }
            Err(e) => {
                let text = e.describe(name);
                crate::log!("{text}");
                tool_result(&text, true)
            }
        };
        Ok(result)
    }
}

fn initialize(params: &Map<String, Value>) -> Value {
    let requested = params.get("protocolVersion").and_then(Value::as_str).unwrap_or("none");
    let version = LEGACY_VERSIONS.iter().copied().find(|v| *v == requested).unwrap_or(LEGACY_VERSIONS[0]);
    let client = params.get("clientInfo").map(client_name).unwrap_or_else(|| "a client".into());
    crate::log!("{client} asks for MCP {requested}, answering with {version}");
    json!({
        "protocolVersion": version,
        "capabilities": capabilities(),
        "serverInfo": server_info(),
        "instructions": INSTRUCTIONS,
    })
}

fn capabilities() -> Value {
    json!({ "tools": { "listChanged": false } })
}

fn server_info() -> Value {
    json!({ "name": "meerora-mcp", "title": "meerora", "version": env!("CARGO_PKG_VERSION") })
}

fn client_name(info: &Value) -> String {
    let field = |key: &str| info.get(key).and_then(Value::as_str).unwrap_or("?");
    format!("{} {}", field("name"), field("version"))
}

fn tool_result(text: &str, is_error: bool) -> Value {
    json!({ "content": [{ "type": "text", "text": text }], "isError": is_error })
}

/// Adds a PNG as MCP image content after the text, for `app_screenshot` (D30).
/// Runtimes that only read text still get the text.
fn with_image(mut result: Value, image: Option<String>) -> Value {
    if let (Some(data), Some(content)) = (image, result["content"].as_array_mut()) {
        content.push(json!({ "type": "image", "data": data, "mimeType": "image/png" }));
    }
    result
}

fn error_response(id: Value, error: RpcError) -> Value {
    let mut body = json!({ "code": error.code, "message": error.message });
    if let Some(data) = error.data {
        body["data"] = data;
    }
    json!({ "jsonrpc": "2.0", "id": id, "error": body })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use meerora_protocol::{Command, ToolCall};

    use super::*;
    use crate::testing::{FakeDaemon, TestDir, answer, write_data_dir};

    fn server(dir: &TestDir) -> Server {
        write_data_dir(dir);
        let daemon = Daemon::new(dir.path("d.sock"), "wren", Duration::from_secs(5));
        Server::new(Tools::load(&dir.0, "wren"), daemon)
    }

    fn send(server: &mut Server, message: Value) -> Value {
        let line = server.handle_line(&message.to_string()).expect("a reply");
        assert!(!line.contains('\n'));
        serde_json::from_str(&line).unwrap()
    }

    fn modern_meta() -> Value {
        json!({ VERSION_KEY: "2026-07-28", CAPABILITIES_KEY: {}, CLIENT_INFO_KEY: { "name": "test", "version": "1" } })
    }

    #[test]
    fn legacy_handshake_negotiates_the_version() {
        let dir = TestDir::new("rpc-init");
        let mut server = server(&dir);
        for version in ["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25"] {
            let reply = send(
                &mut server,
                json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
                "protocolVersion": version, "capabilities": {}, "clientInfo": { "name": "test", "version": "1" } } }),
            );
            assert_eq!(reply["id"], 1);
            assert_eq!(reply["result"]["protocolVersion"], version);
            assert_eq!(reply["result"]["capabilities"]["tools"]["listChanged"], false);
            assert_eq!(reply["result"]["serverInfo"]["name"], "meerora-mcp");
            assert!(reply["result"].get("resultType").is_none());
        }
        for version in ["2023-01-01", "2026-07-28", "2099-01-01"] {
            let reply =
                send(&mut server, json!({ "jsonrpc": "2.0", "id": "a", "method": "initialize", "params": { "protocolVersion": version } }));
            assert_eq!(reply["id"], "a");
            assert_eq!(reply["result"]["protocolVersion"], "2025-11-25");
        }
        assert_eq!(server.handle_line(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#), None);
        assert_eq!(
            send(&mut server, json!({ "jsonrpc": "2.0", "id": 2, "method": "ping" })),
            json!({ "jsonrpc": "2.0", "id": 2, "result": {} })
        );
    }

    #[test]
    fn modern_clients_discover_and_list() {
        let dir = TestDir::new("rpc-modern");
        let mut server = server(&dir);
        let reply =
            send(&mut server, json!({ "jsonrpc": "2.0", "id": 1, "method": "server/discover", "params": { "_meta": modern_meta() } }));
        let result = &reply["result"];
        assert_eq!(result["resultType"], "complete");
        assert_eq!(result["supportedVersions"][0], "2026-07-28");
        assert!(result["supportedVersions"].as_array().unwrap().contains(&json!("2024-11-05")));
        assert_eq!(result["capabilities"]["tools"]["listChanged"], false);
        assert_eq!(result["_meta"][SERVER_INFO_KEY]["name"], "meerora-mcp");
        assert_eq!(result["cacheScope"], "private");

        let reply = send(&mut server, json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": { "_meta": modern_meta() } }));
        assert_eq!(reply["result"]["resultType"], "complete");
        assert_eq!(reply["result"]["tools"].as_array().unwrap().len(), 11);
        assert_eq!(reply["result"]["ttlMs"], CACHE_TTL_MS);

        let reply = send(
            &mut server,
            json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {
            "_meta": modern_meta(), "name": "go_to", "arguments": { "station": "kitchen" } } }),
        );
        assert_eq!(reply["result"]["resultType"], "complete");
        assert_eq!(reply["result"]["isError"], true);
    }

    #[test]
    fn modern_errors() {
        let dir = TestDir::new("rpc-modern-err");
        let mut server = server(&dir);
        let reply = send(
            &mut server,
            json!({ "jsonrpc": "2.0", "id": 1, "method": "server/discover", "params": {
            "_meta": { VERSION_KEY: "2027-01-01", CAPABILITIES_KEY: {} } } }),
        );
        assert_eq!(reply["error"]["code"], -32022);
        assert_eq!(reply["error"]["data"]["requested"], "2027-01-01");
        assert_eq!(reply["error"]["data"]["supported"][0], "2026-07-28");

        let reply = send(
            &mut server,
            json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": { "_meta": { VERSION_KEY: "2026-07-28" } } }),
        );
        assert_eq!(reply["error"]["code"], -32602);

        // A legacy version in _meta is served the legacy way.
        let reply = send(
            &mut server,
            json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/list", "params": { "_meta": { VERSION_KEY: "2025-06-18" } } }),
        );
        assert!(reply["result"].get("resultType").is_none());
    }

    #[test]
    fn json_rpc_errors() {
        let dir = TestDir::new("rpc-errors");
        let mut server = server(&dir);
        let reply = send(&mut server, json!({ "jsonrpc": "2.0", "id": 7, "method": "resources/list" }));
        assert_eq!(reply["error"]["code"], -32601);
        assert_eq!(reply["id"], 7);
        assert_eq!(server.handle_line(r#"{"jsonrpc":"2.0","method":"notifications/whatever","params":{}}"#), None);
        assert_eq!(server.handle_line(r#"{"jsonrpc":"2.0","id":9,"result":{}}"#), None);
        assert_eq!(server.handle_line("   "), None);

        let reply: Value = serde_json::from_str(&server.handle_line("{not json").unwrap()).unwrap();
        assert_eq!(reply["error"]["code"], -32700);
        assert_eq!(reply["id"], Value::Null);
        assert_eq!(send(&mut server, json!({ "id": 1, "method": "ping" }))["error"]["code"], -32600);
        assert_eq!(send(&mut server, json!({ "jsonrpc": "2.0", "id": null, "method": "ping" }))["error"]["code"], -32600);
        assert_eq!(send(&mut server, json!({ "jsonrpc": "2.0", "id": 1, "method": 5 }))["error"]["code"], -32600);
        assert_eq!(send(&mut server, json!([]))["error"]["code"], -32600);
        assert_eq!(send(&mut server, json!({ "jsonrpc": "2.0", "id": 1, "method": "ping", "params": [1] }))["error"]["code"], -32602);

        let batch = send(
            &mut server,
            json!([
                { "jsonrpc": "2.0", "id": 1, "method": "ping" },
                { "jsonrpc": "2.0", "method": "notifications/initialized" },
                { "jsonrpc": "2.0", "id": 2, "method": "nope" },
            ]),
        );
        assert_eq!(batch.as_array().unwrap().len(), 2);
        assert_eq!(batch[1]["error"]["code"], -32601);

        let reply =
            send(&mut server, json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": { "name": "fly", "arguments": {} } }));
        assert_eq!(reply["error"]["code"], -32602);
        assert_eq!(reply["error"]["message"], "Unknown tool fly");
        let reply = send(&mut server, json!({ "jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": { "arguments": {} } }));
        assert_eq!(reply["error"]["code"], -32602);
    }

    #[test]
    fn invalid_arguments_are_tool_errors() {
        let dir = TestDir::new("rpc-invalid");
        let mut server = server(&dir);
        let reply = send(
            &mut server,
            json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {
            "name": "figure_set", "arguments": { "name": "letter", "value": "maybe" } } }),
        );
        assert_eq!(reply["result"]["isError"], true);
        assert_eq!(reply["result"]["content"][0]["text"], "letter takes true or false, not \"maybe\".");
    }

    #[test]
    fn calls_reach_the_daemon() {
        let dir = TestDir::new("rpc-call");
        let mut server = server(&dir);
        let fake = FakeDaemon::start(&dir.path("d.sock"), answer(true, ""));
        let reply = send(
            &mut server,
            json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {
            "name": "figure_set", "arguments": { "name": "letter", "value": true } } }),
        );
        assert_eq!(reply["result"], json!({ "content": [{ "type": "text", "text": "Done." }], "isError": false }));
        let Some(Command::Tool { agent, call, .. }) = fake.next_command(Duration::from_secs(1)) else { panic!("no tool call") };
        assert_eq!(agent, "wren");
        assert_eq!(call, ToolCall::FigureSet { name: "letter".into(), value: json!(true) });

        let reply = send(
            &mut server,
            json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {
            "name": "ask_user", "arguments": { "question": "Archive the newsletter?" } } }),
        );
        assert_eq!(reply["result"]["content"][0]["text"], "The user sees your question. End your turn now.");
    }

    /// A screenshot reaches the model as MCP image content after the text,
    /// and an app tool call reaches the daemon as its tool call (D30).
    #[test]
    fn app_tools_reach_the_daemon_and_screenshots_are_images() {
        use crate::testing::Reaction;
        let dir = TestDir::new("rpc-apps");
        let mut server = server(&dir);
        let fake = FakeDaemon::start(&dir.path("d.sock"), |command: &Command| match command {
            Command::Tool { id, call: ToolCall::AppScreenshot { app }, .. } => Reaction::Reply(vec![meerora_protocol::Event::ToolResult {
                id: id.clone(),
                ok: true,
                message: format!("A picture of {app}."),
                image: Some("iVBORw0KGgo=".into()),
            }]),
            other => answer(true, "Ran it.")(other),
        });
        let reply = send(
            &mut server,
            json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {
            "name": "app_screenshot", "arguments": { "app": "browser:1" } } }),
        );
        assert_eq!(
            reply["result"],
            json!({ "content": [
                { "type": "text", "text": "A picture of browser:1." },
                { "type": "image", "data": "iVBORw0KGgo=", "mimeType": "image/png" },
            ], "isError": false })
        );
        let reply = send(
            &mut server,
            json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {
            "name": "app_act", "arguments": { "app": "terminal:1", "action": "run", "args": { "command": "echo hi" } } } }),
        );
        assert_eq!(reply["result"], json!({ "content": [{ "type": "text", "text": "Ran it." }], "isError": false }));
        let calls: Vec<ToolCall> = std::iter::from_fn(|| fake.next_command(Duration::from_secs(1)))
            .filter_map(|c| match c {
                Command::Tool { call, .. } => Some(call),
                _ => None,
            })
            .collect();
        assert_eq!(
            calls,
            [
                ToolCall::AppScreenshot { app: "browser:1".into() },
                ToolCall::AppAct { app: "terminal:1".into(), action: "run".into(), args: json!({ "command": "echo hi" }) },
            ]
        );
    }

    #[test]
    fn refusals_and_a_missing_daemon_are_tool_errors() {
        let dir = TestDir::new("rpc-refuse");
        let mut server = server(&dir);
        let call =
            json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": "note", "arguments": { "text": "hi" } } });

        let reply = send(&mut server, call.clone());
        assert_eq!(reply["result"]["isError"], true);
        let text = reply["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.starts_with("meerora is not running, so note had no effect."), "{text}");

        let _fake = FakeDaemon::start(&dir.path("d.sock"), answer(false, "Wren has no run."));
        let reply = send(&mut server, call);
        assert_eq!(reply["result"], json!({ "content": [{ "type": "text", "text": "Wren has no run." }], "isError": true }));
    }

    #[test]
    fn the_real_assets_build() {
        let assets = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets");
        let defs = Tools::load(&assets, "wren").definitions();
        let schema = |tool: &str| defs.iter().find(|d| d["name"] == tool).unwrap()["inputSchema"]["properties"].clone();
        assert!(schema("figure_set")["name"]["enum"].as_array().unwrap().contains(&json!("letter")));
        assert!(schema("go_to")["station"]["enum"].as_array().unwrap().contains(&json!("mailbox")));
    }
}
