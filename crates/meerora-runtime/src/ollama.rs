//! Ollama as a model runtime (D22). This crate runs the agent loop: it sends
//! the conversation to `POST /api/chat` with streaming and tools, runs the
//! tool calls and repeats until the model answers without tool calls, at
//! most [`MAX_TURNS`] times.
//!
//! The conversation is kept in `<dir>/logs/sessions/<id>.json` (D21, not
//! committed). The system prompt is not stored there, every run sends the
//! current one.
//!
//! One answer of the server may bring at most [`MAX_RESPONSE`] bytes in lines
//! of at most [`MAX_LINE`], with [`MAX_REPLY`] of text and [`MAX_TOOL_CALLS`]
//! tool calls. Only the first [`MAX_ERROR_BODY`] bytes of an error are read.

use std::collections::HashSet;
use std::hash::{BuildHasher, RandomState};
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use meerora_protocol::{RuntimeConfig, Settings};
use serde_json::{Value, json};

use crate::actions::{MEERORA_MCP_PREFIX, action};
use crate::bounded::{self, size};
use crate::mcp::McpClient;
use crate::process::{self, CancelState, Emitter};
use crate::sandbox::{self, Mounts};
use crate::tools::{self, AgentFiles};
use crate::{RunEvent, RunHandle, RunRequest, Runtime};

pub const DEFAULT_URL: &str = "http://localhost:11434";
/// Most model calls in one run.
pub const MAX_TURNS: usize = 12;
/// The most bytes one streamed answer may bring.
const MAX_RESPONSE: usize = 32 * 1024 * 1024;
/// The longest line of a streamed answer.
const MAX_LINE: usize = 1024 * 1024;
/// The most text of one reply.
const MAX_REPLY: usize = 4 * 1024 * 1024;
/// The most tool calls of one reply.
const MAX_TOOL_CALLS: usize = 64;
/// How much of an error answer is read.
const MAX_ERROR_BODY: u64 = 64 * 1024;

pub(crate) struct Ollama {
    url: String,
    model: Option<String>,
}

impl Ollama {
    /// The URL comes from the agent's `[runtime] url`, then `paths.ollama`
    /// in `meerora.toml`, then [`DEFAULT_URL`].
    pub fn new(config: &RuntimeConfig, settings: &Settings) -> Self {
        let url = config.url.clone().or_else(|| settings.paths.ollama.clone()).unwrap_or_else(|| DEFAULT_URL.to_string());
        Self { url: url.trim_end_matches('/').to_string(), model: config.model.clone() }
    }
}

impl Runtime for Ollama {
    fn name(&self) -> &'static str {
        "ollama"
    }

    fn start(&self, request: RunRequest) -> anyhow::Result<RunHandle> {
        let (tx, rx) = mpsc::channel();
        let emitter = Emitter::new(tx);
        let cancel = Arc::new(CancelState::default());
        let model = request.model.clone().or_else(|| self.model.clone());
        let mut run = Loop { url: self.url.clone(), emitter: emitter.clone(), cancel: cancel.clone(), usage: None };
        std::thread::Builder::new().name(format!("ollama-{}", request.agent)).spawn(move || {
            let result = run.run(model, request);
            if let Some((input_tokens, output_tokens)) = run.usage {
                run.emitter.emit(RunEvent::Usage { input_tokens, output_tokens });
            }
            run.emitter.finish(result);
        })?;
        Ok(RunHandle::new(
            rx,
            Box::new(move || {
                cancel.cancel();
                emitter.finish(Err("cancelled".to_string()));
            }),
        ))
    }
}

/// One model reply, assembled from the stream.
#[derive(Debug, Default)]
struct Reply {
    content: String,
    tool_calls: Vec<Value>,
    prompt_tokens: u64,
    output_tokens: u64,
}

struct Loop {
    url: String,
    emitter: Arc<Emitter>,
    cancel: Arc<CancelState>,
    usage: Option<(u64, u64)>,
}

impl Loop {
    fn check_cancel(&self) -> Result<(), String> {
        if self.cancel.is_cancelled() { Err("cancelled".to_string()) } else { Ok(()) }
    }

    fn run(&mut self, model: Option<String>, request: RunRequest) -> Result<String, String> {
        let model = model.ok_or("no model set for the ollama runtime (set model in [runtime])")?;
        process::check_dir(&request.dir)?;
        let workspace = request.isolation.as_ref().map(|i| i.workspace.clone()).unwrap_or_default();
        let mut files = AgentFiles::new(&request.dir)?.with_workspace(&workspace);
        // Commands run in the sandbox (D16). It sees no daemon socket, the meerora tools go through the bridge.
        if let Some(isolation) = request.isolation.as_ref().filter(|i| i.sandbox && request.permissions.shell) {
            let bwrap = sandbox::find()?;
            let mounts = Mounts {
                dir: files.root().to_path_buf(),
                read: isolation.read.clone(),
                hide: sandbox::user_home(),
                workspace,
                ..Mounts::default()
            };
            sandbox::probe(&bwrap, &mounts)?;
            files = files.sandboxed(bwrap, mounts);
        }
        let (id, mut history) = match &request.session {
            Some(id) => {
                check_session_id(id)?;
                (id.clone(), load_session(&request.dir, id)?)
            }
            None => (new_session_id(), Vec::new()),
        };
        self.emitter.emit(RunEvent::Session { id: id.clone() });

        let mut mcp = match &request.mcp {
            Some(bridge) => {
                let client = McpClient::start(bridge, files.root())?;
                self.cancel.register(client.pgid());
                Some(client)
            }
            None => None,
        };
        let mut offered = tools::definitions(request.permissions);
        let mut mcp_names = HashSet::new();
        if let Some(client) = mcp.as_mut() {
            for tool in client.list_tools()? {
                if tools::is_builtin(&tool.name) {
                    tracing::warn!("the MCP tool {} has the name of a built-in tool and is not offered", tool.name);
                    continue;
                }
                offered.push(tools::function(&tool.name, &tool.description, tool.input_schema));
                mcp_names.insert(tool.name);
            }
        }

        history.push(json!({ "role": "user", "content": request.prompt }));
        let http = http_agent();
        let result = (|| -> Result<String, String> {
            for _ in 0..MAX_TURNS {
                self.check_cancel()?;
                let mut messages = Vec::with_capacity(history.len() + 1);
                if !request.system.trim().is_empty() {
                    messages.push(json!({ "role": "system", "content": request.system }));
                }
                messages.extend(history.iter().cloned());
                let body = json!({ "model": model, "messages": messages, "tools": offered, "stream": true });
                let reply = self.chat(&http, &model, &body)?;
                let (input, output) = self.usage.unwrap_or((0, 0));
                self.usage = Some((input + reply.prompt_tokens, output + reply.output_tokens));

                let text = strip_think(&reply.content);
                if !text.is_empty() {
                    self.emitter.emit(RunEvent::Text { text: text.clone() });
                }
                let mut message = json!({ "role": "assistant", "content": text });
                if !reply.tool_calls.is_empty() {
                    message["tool_calls"] = Value::Array(reply.tool_calls.clone());
                }
                history.push(message);
                if reply.tool_calls.is_empty() {
                    return Ok(text);
                }
                for call in &reply.tool_calls {
                    self.check_cancel()?;
                    let name = call.pointer("/function/name").and_then(Value::as_str).unwrap_or("");
                    let args = arguments(call.pointer("/function/arguments"));
                    let is_mcp = mcp_names.contains(name);
                    let logged = if is_mcp { format!("{MEERORA_MCP_PREFIX}{name}") } else { name.to_string() };
                    self.emitter.emit(action(&logged, &args, files.root()));
                    let output = match mcp.as_mut() {
                        Some(client) if is_mcp => client.call(name, &args),
                        _ if tools::is_builtin(name) => tools::call(&files, request.permissions, name, &args, &self.cancel),
                        _ => Err(format!("there is no tool called {name}")),
                    };
                    let content = output.unwrap_or_else(|e| format!("error: {e}"));
                    let mut answer = json!({ "role": "tool", "tool_name": name, "content": content });
                    if let Some(call_id) = call.get("id") {
                        answer["tool_call_id"] = call_id.clone();
                    }
                    history.push(answer);
                }
                save_session(&request.dir, &id, &model, &history)?;
            }
            Err(format!("{model} was still calling tools after {MAX_TURNS} turns"))
        })();
        let saved = save_session(&request.dir, &id, &model, &history);
        if let Some(client) = &mcp {
            self.cancel.unregister(client.pgid());
        }
        let answer = result?;
        saved?;
        Ok(answer)
    }

    fn chat(&self, http: &ureq::Agent, model: &str, body: &Value) -> Result<Reply, String> {
        let url = &self.url;
        let response = http.post(&format!("{url}/api/chat")).send_json(body).map_err(|e| transport_error(url, &e))?;
        let status = response.status().as_u16();
        if status != 200 {
            let text = bounded::read_prefix(response.into_body().into_reader(), MAX_ERROR_BODY);
            let message = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v.get("error").and_then(Value::as_str).map(str::to_string))
                .unwrap_or(text);
            if status == 404 && message.contains("not found") {
                return Err(format!("model {model} not found (run: ollama pull {model})"));
            }
            return Err(format!("ollama answered {status}: {}", message.trim()));
        }
        let mut reply = Reply::default();
        let mut reader = BufReader::new(response.into_body().into_reader());
        let (mut buf, mut total) = (Vec::new(), 0usize);
        loop {
            self.check_cancel()?;
            let line = match bounded::read_line(&mut reader, MAX_LINE, &mut buf) {
                Ok(Some(line)) => line,
                Ok(None) => break,
                Err(e) if e.kind() == std::io::ErrorKind::InvalidData => return Err(format!("ollama at {url} sent {e}")),
                Err(e) => return Err(format!("lost the connection to ollama at {url}: {e}")),
            };
            total += line.len() + 1;
            if total > MAX_RESPONSE {
                return Err(format!("ollama at {url} sent more than {} in one answer", size(MAX_RESPONSE)));
            }
            if line.trim().is_empty() {
                continue;
            }
            let chunk: Value = serde_json::from_str(&line).map_err(|e| format!("ollama sent a line that is not JSON: {e}"))?;
            if let Some(error) = chunk.get("error").and_then(Value::as_str) {
                return Err(format!("ollama: {error}"));
            }
            if let Some(message) = chunk.get("message") {
                if let Some(text) = message.get("content").and_then(Value::as_str) {
                    reply.content.push_str(text);
                }
                if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
                    reply.tool_calls.extend(calls.iter().cloned());
                }
                if reply.content.len() > MAX_REPLY {
                    return Err(format!("{model} wrote more than {} in one reply", size(MAX_REPLY)));
                }
                if reply.tool_calls.len() > MAX_TOOL_CALLS {
                    return Err(format!("{model} asked for more than {MAX_TOOL_CALLS} tool calls in one reply"));
                }
            }
            if chunk.get("done").and_then(Value::as_bool) == Some(true) {
                reply.prompt_tokens = chunk.get("prompt_eval_count").and_then(Value::as_u64).unwrap_or(0);
                reply.output_tokens = chunk.get("eval_count").and_then(Value::as_u64).unwrap_or(0);
                break;
            }
        }
        Ok(reply)
    }
}

fn http_agent() -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_connect(Some(Duration::from_secs(5)))
        // Loading a model can take minutes on a slow disk.
        .timeout_recv_response(Some(Duration::from_secs(600)))
        .build();
    ureq::Agent::new_with_config(config)
}

fn transport_error(url: &str, error: &ureq::Error) -> String {
    match error {
        ureq::Error::ConnectionFailed | ureq::Error::HostNotFound => format!("ollama is not running at {url}"),
        ureq::Error::Io(e) if matches!(e.kind(), std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::ConnectionReset) => {
            format!("ollama is not running at {url}")
        }
        ureq::Error::Timeout(_) => format!("ollama at {url} did not answer in time"),
        other => format!("cannot reach ollama at {url}: {other}"),
    }
}

/// Tool arguments arrive as an object, or from some models as a JSON string.
fn arguments(raw: Option<&Value>) -> Value {
    match raw {
        Some(Value::Object(_)) => raw.cloned().unwrap_or_default(),
        Some(Value::String(s)) => serde_json::from_str(s).unwrap_or_else(|_| json!({})),
        _ => json!({}),
    }
}

/// Removes `<think>...</think>` blocks that qwen3 and similar models put
/// into their text. A reply that starts inside a block, when the opening tag
/// was part of the prompt template, loses everything up to `</think>`.
pub(crate) fn strip_think(text: &str) -> String {
    const OPEN: &str = "<think>";
    const CLOSE: &str = "</think>";
    let mut rest = text;
    if let Some(end) = rest.find(CLOSE)
        && !rest[..end].contains(OPEN)
    {
        rest = &rest[end + CLOSE.len()..];
    }
    let mut out = String::new();
    while let Some(start) = rest.find(OPEN) {
        out.push_str(&rest[..start]);
        rest = match rest[start..].find(CLOSE) {
            Some(end) => &rest[start + end + CLOSE.len()..],
            None => "",
        };
    }
    out.push_str(rest);
    out.trim().to_string()
}

fn new_session_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let noise = RandomState::new().hash_one((now.as_nanos(), std::process::id(), COUNTER.fetch_add(1, Ordering::Relaxed)));
    format!("{:x}-{:08x}", now.as_secs(), noise as u32)
}

fn check_session_id(id: &str) -> Result<(), String> {
    let ok = !id.is_empty() && id.len() <= 100 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if ok { Ok(()) } else { Err(format!("{id:?} is not an ollama session id")) }
}

pub(crate) fn session_path(dir: &Path, id: &str) -> PathBuf {
    dir.join("logs").join("sessions").join(format!("{id}.json"))
}

/// Messages of an earlier run. A missing file starts the session afresh.
fn load_session(dir: &Path, id: &str) -> Result<Vec<Value>, String> {
    let path = session_path(dir, id);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tracing::warn!("ollama session {id} has no file at {}, starting it afresh", path.display());
            return Ok(Vec::new());
        }
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    let session: Value = serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(session.get("messages").and_then(Value::as_array).cloned().unwrap_or_default())
}

fn save_session(dir: &Path, id: &str, model: &str, messages: &[Value]) -> Result<(), String> {
    let path = session_path(dir, id);
    let folder = path.parent().unwrap_or(dir);
    std::fs::create_dir_all(folder).map_err(|e| format!("cannot create {}: {e}", folder.display()))?;
    let text = serde_json::to_string_pretty(&json!({ "id": id, "model": model, "messages": messages })).unwrap_or_default();
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text).and_then(|_| std::fs::rename(&tmp, &path)).map_err(|e| format!("cannot save {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn think_blocks_are_removed() {
        assert_eq!(strip_think("<think>\nhmm\n</think>\n\nHello."), "Hello.");
        assert_eq!(strip_think("A <think>x</think>B<think>y</think> C"), "A B C");
        assert_eq!(strip_think("reasoning</think>Answer"), "Answer");
        assert_eq!(strip_think("<think>never closed"), "");
        assert_eq!(strip_think("plain"), "plain");
    }

    #[test]
    fn session_ids_are_file_names() {
        let id = new_session_id();
        assert!(check_session_id(&id).is_ok(), "{id}");
        assert_ne!(id, new_session_id());
        assert!(check_session_id("../etc").is_err());
        assert!(check_session_id("").is_err());
    }

    /// A fake ollama whose answer is `body` with `status`.
    fn fake(status: u16, body: Vec<u8>) -> meerora_index::http::Server {
        meerora_index::http::Server::start(
            "127.0.0.1:0",
            Arc::new(move |_req: meerora_index::http::Request| {
                meerora_index::http::Response::bytes(status, "application/x-ndjson", body.clone())
            }),
        )
        .unwrap()
    }

    fn chat(server: &meerora_index::http::Server) -> Result<Reply, String> {
        let (tx, _rx) = mpsc::channel();
        let run = Loop { url: server.url(), emitter: Emitter::new(tx), cancel: Arc::new(CancelState::default()), usage: None };
        run.chat(&http_agent(), "m", &json!({}))
    }

    #[test]
    fn answers_and_errors_have_limits() {
        let ok = fake(200, b"{\"message\":{\"content\":\"Hi\"}}\n{\"done\":true,\"eval_count\":1}\n".to_vec());
        assert_eq!(chat(&ok).unwrap().content, "Hi");

        let long_line = fake(200, vec![b'x'; 2 * 1024 * 1024]);
        let err = chat(&long_line).unwrap_err();
        assert!(err.contains("a line is longer than 1 MiB"), "{err}");

        let chunk = format!("{{\"message\":{{\"content\":\"{}\"}}}}\n", "y".repeat(512 * 1024));
        let endless = fake(200, chunk.repeat(10).into_bytes());
        let err = chat(&endless).unwrap_err();
        assert!(err.contains("more than 4 MiB in one reply"), "{err}");

        let mut error = b"{\"error\":\"".to_vec();
        error.extend(std::iter::repeat_n(b'e', 3 * 1024 * 1024));
        let err = chat(&fake(500, error)).unwrap_err();
        assert!(err.starts_with("ollama answered 500") && err.len() < 70 * 1024, "{}", err.len());
    }

    #[test]
    fn arguments_accept_strings() {
        assert_eq!(arguments(Some(&json!("{\"path\":\"a\"}"))), json!({"path": "a"}));
        assert_eq!(arguments(Some(&json!({"path": "b"}))), json!({"path": "b"}));
        assert_eq!(arguments(None), json!({}));
    }
}
