//! External agents from the meerora index (D22, D25) over A2A 1.0 (Q9).
//!
//! The agent lives on its owner's server. This runtime is only a client of
//! the A2A JSON-RPC binding: it sends what the user typed and reads the
//! answer. It never sends the system prompt, the agent directory, files or
//! tools, and it starts no MCP bridge (D25). The only local file it reads is
//! the agent card that the import saved in `config/agent-card.json`, to learn
//! whether the agent streams.
//!
//! Implemented from the A2A specification 1.0.0
//! (<https://a2a-protocol.org/latest/specification/>, read on 14 Oct 2026)
//! and `specification/a2a.proto` of <https://github.com/a2aproject/A2A>:
//!
//! - `SendStreamingMessage`, answered with server-sent events whose `data`
//!   is a JSON-RPC response with a `StreamResponse` (`task`, `message`,
//!   `statusUpdate` or `artifactUpdate`) as its result (sections 3.1.2, 9.4.2).
//! - `SendMessage` when the card says `capabilities.streaming` is not true,
//!   or when the agent answers the streaming call with
//!   `UnsupportedOperationError` (-32004) or method not found (-32601).
//! - `CancelTask` when the user stops a run, as a courtesy.
//! - Every request carries `A2A-Version: 1.0` (section 3.6.1). Messages use
//!   `ROLE_USER`, parts are `{"text": ...}`, enums are ProtoJSON names such
//!   as `TASK_STATE_INPUT_REQUIRED`. Lower-case names (`input-required`,
//!   `agent`) are accepted too.
//!
//! Mapping to [`RunEvent`]s:
//!
//! - The `contextId` becomes [`RunEvent::Session`], so the next run continues
//!   the conversation. While a task waits for input the session also holds
//!   its `taskId`, and the next message continues that task (section 3.4).
//! - Text arrives as blocks: a whole artifact (its chunks joined, emitted at
//!   `lastChunk` or at the end), or a message. Status messages of a working
//!   task are only used when nothing else came.
//! - `TASK_STATE_COMPLETED`, a direct message or the end of the stream become
//!   `Finished(Ok)`. `TASK_STATE_INPUT_REQUIRED` becomes `Finished(Ok)` with
//!   the agent's question as the answer, so the agent waits for the user.
//!   Failed, rejected and cancelled tasks become `Finished(Err)`.
//!
//! The endpoint belongs to someone else, so everything it sends has a limit:
//! a plain answer [`MAX_ANSWER`], a whole event stream [`MAX_STREAM`], one
//! line of it [`MAX_LINE`] and one event [`MAX_EVENT`]. The text a run holds
//! (open artifacts and finished blocks) stays below [`MAX_TEXT`], with at
//! most [`MAX_OPEN`] artifacts open and [`MAX_BLOCKS`] blocks. Beyond that the
//! run ends with an error that names the limit.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use meerora_protocol::RuntimeConfig;
use serde_json::{Value, json};
use tracing::{debug, info};

use crate::bounded::{self, size};
use crate::{RunEvent, RunHandle, RunRequest, Runtime};

/// The largest answer without streaming.
const MAX_ANSWER: usize = 4 * 1024 * 1024;
/// The most bytes one event stream may send.
const MAX_STREAM: usize = 16 * 1024 * 1024;
/// The longest line of an event stream.
const MAX_LINE: usize = 1024 * 1024;
/// The largest data of one event, all its `data:` lines together.
const MAX_EVENT: usize = 4 * 1024 * 1024;
/// The most text one run holds, in open artifacts and blocks together.
const MAX_TEXT: usize = 4 * 1024 * 1024;
/// Artifacts that may receive chunks at the same time.
const MAX_OPEN: usize = 64;
/// Text blocks one run may show.
const MAX_BLOCKS: usize = 1000;
/// How much of an error answer is read.
const MAX_ERROR_BODY: u64 = 64 * 1024;

/// Version sent in the `A2A-Version` header.
pub const A2A_VERSION: &str = "1.0";
/// Where the import keeps the agent card, relative to the agent directory.
pub const CARD_FILE: &str = "config/agent-card.json";

const UNSUPPORTED_OPERATION: i64 = -32004;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const TASK_NOT_FOUND: i64 = -32001;

pub(crate) struct External {
    url: Option<String>,
}

impl External {
    /// The endpoint is the agent's `[runtime] url`, which the import sets to
    /// the manifest's `endpoint.url`.
    pub fn new(config: &RuntimeConfig) -> Self {
        Self { url: config.url.clone().filter(|u| !u.trim().is_empty()) }
    }
}

impl Runtime for External {
    fn name(&self) -> &'static str {
        "external"
    }

    fn start(&self, request: RunRequest) -> anyhow::Result<RunHandle> {
        let (tx, rx) = mpsc::channel();
        let out = Out::new(tx);
        let cancelled = Arc::new(AtomicBool::new(false));
        let task: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let Some(url) = self.url.clone() else {
            out.finish(Err("This external agent has no endpoint. Its agent.toml needs [runtime] url.".into()));
            return Ok(RunHandle::new(rx, Box::new(|| {})));
        };
        let client = Client { url: url.clone(), out: out.clone(), cancelled: cancelled.clone(), task: task.clone() };
        std::thread::Builder::new().name(format!("a2a-{}", request.agent)).spawn(move || {
            let result = client.run(&request);
            client.out.finish(result);
        })?;
        let cancel = move || {
            if cancelled.swap(true, Ordering::SeqCst) {
                return;
            }
            out.finish(Err("cancelled".into()));
            let pending = task.lock().ok().and_then(|t| t.clone());
            if let Some(task) = pending {
                let url = url.clone();
                let _ = std::thread::Builder::new().name("a2a-cancel".into()).spawn(move || {
                    let body = rpc("CancelTask", json!({ "id": task }));
                    let _ = http(Duration::from_secs(5)).post(&url).header("A2A-Version", A2A_VERSION).send_json(&body);
                });
            }
        };
        Ok(RunHandle::new(rx, Box::new(cancel)))
    }
}

/// Sends run events until `Finished`, which goes out exactly once.
#[derive(Clone)]
struct Out(Arc<Mutex<Option<Sender<RunEvent>>>>);

impl Out {
    fn new(tx: Sender<RunEvent>) -> Self {
        Self(Arc::new(Mutex::new(Some(tx))))
    }

    fn send(&self, event: RunEvent) {
        if let Some(tx) = self.0.lock().ok().as_ref().and_then(|g| g.as_ref()) {
            let _ = tx.send(event);
        }
    }

    fn finish(&self, result: Result<String, String>) {
        if let Some(tx) = self.0.lock().ok().and_then(|mut g| g.take()) {
            let _ = tx.send(RunEvent::Finished { result });
        }
    }
}

/// The conversation to continue: `contextId`, plus the `taskId` of a task
/// that waits for input. Stored as the plain context id, or as JSON when a
/// task waits.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Session {
    pub context: Option<String>,
    pub task: Option<String>,
}

impl Session {
    pub fn parse(id: Option<&str>) -> Self {
        let Some(id) = id.map(str::trim).filter(|s| !s.is_empty()) else { return Self::default() };
        if id.starts_with('{')
            && let Ok(v) = serde_json::from_str::<Value>(id)
        {
            let s = |key: &str| v.get(key).and_then(Value::as_str).map(str::to_string);
            return Self { context: s("contextId"), task: s("taskId") };
        }
        Self { context: Some(id.to_string()), task: None }
    }

    /// The id for [`RunEvent::Session`], None without a context.
    pub fn id(&self) -> Option<String> {
        let context = self.context.as_ref()?;
        Some(match &self.task {
            Some(task) => json!({ "contextId": context, "taskId": task }).to_string(),
            None => context.clone(),
        })
    }
}

/// How a task ended, from its last status.
#[derive(Clone, Debug, PartialEq)]
enum End {
    Completed,
    /// The agent asks the user, the task stays open.
    Waiting,
    Failed(String),
    Cancelled,
}

/// What a run collected so far.
#[derive(Default)]
struct Progress {
    session: Session,
    /// The session id last sent as [`RunEvent::Session`].
    reported: Option<String>,
    /// Text blocks sent as [`RunEvent::Text`].
    blocks: Vec<String>,
    /// Artifacts still receiving chunks: (id, text).
    open: VecDeque<(String, String)>,
    /// The last status text of a working task, used when nothing else came.
    status_text: Option<String>,
    end: Option<End>,
}

enum Failure {
    /// The agent cannot stream, use `SendMessage`.
    NoStreaming,
    /// The agent does not know the task the session points at.
    StaleTask(String),
    Other(String),
}

struct Client {
    url: String,
    out: Out,
    cancelled: Arc<AtomicBool>,
    /// The task of this run, for `CancelTask`.
    task: Arc<Mutex<Option<String>>>,
}

impl Client {
    fn run(&self, request: &RunRequest) -> Result<String, String> {
        let mut session = Session::parse(request.session.as_deref());
        let streams = card_streams(&request.dir);
        debug!("{} talks to {} ({})", request.agent, self.url, if streams == Some(false) { "no streaming" } else { "streaming" });
        let mut progress = Progress { session: session.clone(), ..Progress::default() };
        let mut stream = streams != Some(false);
        // At most three tries: without streaming, and without a task the agent forgot.
        for _ in 0..3 {
            let sent = self.send(&request.prompt, &session, stream, &mut progress);
            match sent {
                Ok(()) => return self.finish(progress),
                Err(Failure::NoStreaming) if stream => {
                    info!("{} does not stream, sending without streaming", self.host());
                    stream = false;
                }
                Err(Failure::StaleTask(e)) if session.task.is_some() => {
                    info!("{} no longer knows the task of this conversation ({e}), starting a new one", self.host());
                    session.task = None;
                    progress = Progress { session: session.clone(), ..Progress::default() };
                }
                Err(Failure::NoStreaming) => return Err(format!("{} refused the message.", self.host())),
                Err(Failure::StaleTask(e) | Failure::Other(e)) => {
                    self.report_session(&mut progress);
                    return Err(e);
                }
            }
        }
        Err(format!("{} did not take the message.", self.host()))
    }

    fn host(&self) -> String {
        self.url.split("://").nth(1).unwrap_or(&self.url).split('/').next().unwrap_or(&self.url).to_string()
    }

    fn send(&self, prompt: &str, session: &Session, stream: bool, progress: &mut Progress) -> Result<(), Failure> {
        let mut message = json!({
            "messageId": next_id("msg"),
            "role": "ROLE_USER",
            "parts": [{ "text": prompt }],
        });
        if let Some(context) = &session.context {
            message["contextId"] = json!(context);
        }
        if let Some(task) = &session.task {
            message["taskId"] = json!(task);
        }
        let params = json!({
            "message": message,
            "configuration": { "acceptedOutputModes": ["text/plain", "text/markdown"] },
        });
        let body = rpc(if stream { "SendStreamingMessage" } else { "SendMessage" }, params);
        let response = http(Duration::from_secs(120))
            .post(&self.url)
            .header("A2A-Version", A2A_VERSION)
            .header("Accept", if stream { "text/event-stream" } else { "application/json" })
            .send_json(&body)
            .map_err(|e| Failure::Other(transport_error(&self.host(), &e)))?;
        let status = response.status().as_u16();
        let event_stream =
            response.headers().get("content-type").and_then(|v| v.to_str().ok()).is_some_and(|v| v.starts_with("text/event-stream"));
        if status == 405 || status == 501 {
            return Err(if stream { Failure::NoStreaming } else { Failure::Other(format!("{} answered {status}.", self.host())) });
        }
        if status != 200 {
            let text = bounded::read_prefix(response.into_body().into_reader(), MAX_ERROR_BODY);
            return Err(Failure::Other(format!("{} answered {status}: {}", self.host(), first_line(&text, 200))));
        }
        if event_stream {
            let reader = BufReader::new(response.into_body().into_reader());
            return self.read_events(reader, progress);
        }
        let mut body = response.into_body();
        let text = body
            .with_config()
            .limit(MAX_ANSWER as u64)
            .read_to_string()
            .map_err(|e| Failure::Other(format!("cannot read the answer of {} ({e})", self.host())))?;
        let envelope: Value =
            serde_json::from_str(&text).map_err(|e| Failure::Other(format!("{} did not answer with JSON-RPC ({e})", self.host())))?;
        let result = self.result_of(&envelope)?;
        self.apply(&result, progress);
        self.check_limits(progress)
    }

    /// Refuses a run whose text, open artifacts or blocks grew past their limits.
    fn check_limits(&self, progress: &Progress) -> Result<(), Failure> {
        let held: usize = progress.open.iter().map(|(_, t)| t.len()).chain(progress.blocks.iter().map(String::len)).sum();
        let what = if held > MAX_TEXT {
            format!("more than {} of text", size(MAX_TEXT))
        } else if progress.open.len() > MAX_OPEN {
            format!("more than {MAX_OPEN} artifacts at once")
        } else if progress.blocks.len() > MAX_BLOCKS {
            format!("more than {MAX_BLOCKS} blocks of text")
        } else {
            return Ok(());
        };
        Err(self.too_much(&what))
    }

    fn too_much(&self, what: &str) -> Failure {
        Failure::Other(format!("{} sent {what}, so meerora stopped reading the answer.", self.host()))
    }

    /// The `result` of a JSON-RPC response, or its error as a failure.
    fn result_of(&self, envelope: &Value) -> Result<Value, Failure> {
        if let Some(error) = envelope.get("error") {
            let code = error.get("code").and_then(Value::as_i64).unwrap_or(0);
            let message = error.get("message").and_then(Value::as_str).unwrap_or("unknown error");
            return Err(match code {
                UNSUPPORTED_OPERATION | METHOD_NOT_FOUND => Failure::NoStreaming,
                TASK_NOT_FOUND | INVALID_PARAMS => Failure::StaleTask(format!("{} refused the message: {message}", self.host())),
                _ => Failure::Other(format!("{} answered with an error: {message} ({code})", self.host())),
            });
        }
        envelope.get("result").cloned().ok_or_else(|| Failure::Other(format!("{} answered without a result", self.host())))
    }

    /// Reads server-sent events until the task ends or the stream closes.
    fn read_events(&self, mut reader: impl BufRead, progress: &mut Progress) -> Result<(), Failure> {
        let mut data = String::new();
        let mut buf = Vec::new();
        let mut total = 0usize;
        loop {
            if self.cancelled.load(Ordering::SeqCst) {
                return Ok(());
            }
            let line = match bounded::read_line(&mut reader, MAX_LINE, &mut buf) {
                Ok(Some(line)) => line,
                Ok(None) => return Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::InvalidData => return Err(self.too_much(&e.to_string())),
                Err(e) => return Err(Failure::Other(format!("lost the connection to {} ({e})", self.host()))),
            };
            total += line.len() + 1;
            if total > MAX_STREAM {
                return Err(self.too_much(&format!("more than {}", size(MAX_STREAM))));
            }
            if line.is_empty() {
                if !data.is_empty() {
                    let envelope: Value = serde_json::from_str(&data)
                        .map_err(|e| Failure::Other(format!("{} sent an event that is not JSON ({e})", self.host())))?;
                    data.clear();
                    let result = self.result_of(&envelope)?;
                    self.apply(&result, progress);
                    self.check_limits(progress)?;
                    if progress.end.is_some() {
                        return Ok(());
                    }
                }
                continue;
            }
            if let Some(rest) = line.strip_prefix("data:") {
                let rest = rest.strip_prefix(' ').unwrap_or(rest);
                if data.len() + rest.len() + 1 > MAX_EVENT {
                    return Err(self.too_much(&format!("an event larger than {}", size(MAX_EVENT))));
                }
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(rest);
            }
            // `event:`, `id:`, `retry:` and comments carry nothing A2A needs.
        }
    }

    /// Applies one `StreamResponse`, or a `SendMessageResponse` (which has
    /// the same `task` and `message` fields).
    fn apply(&self, result: &Value, progress: &mut Progress) {
        if let Some(task) = result.get("task") {
            self.ids(task.get("contextId"), task.get("id"), progress);
            for artifact in task.get("artifacts").and_then(Value::as_array).into_iter().flatten() {
                self.artifact(artifact, false, true, progress);
            }
            if let Some(status) = task.get("status") {
                self.status(status, progress);
            }
        } else if let Some(message) = result.get("message") {
            self.ids(message.get("contextId"), message.get("taskId"), progress);
            self.block(text_of(message), progress);
            // A message ends the exchange (section 3.1.2).
            progress.end = Some(End::Completed);
        } else if let Some(update) = result.get("statusUpdate") {
            self.ids(update.get("contextId"), update.get("taskId"), progress);
            if let Some(status) = update.get("status") {
                self.status(status, progress);
            }
        } else if let Some(update) = result.get("artifactUpdate") {
            self.ids(update.get("contextId"), update.get("taskId"), progress);
            let append = update.get("append").and_then(Value::as_bool).unwrap_or(false);
            let last = update.get("lastChunk").and_then(Value::as_bool).unwrap_or(false);
            if let Some(artifact) = update.get("artifact") {
                self.artifact(artifact, append, last, progress);
            }
        } else {
            debug!("ignoring an answer of {} without task, message or update", self.host());
        }
    }

    fn ids(&self, context: Option<&Value>, task: Option<&Value>, progress: &mut Progress) {
        if let Some(context) = context.and_then(Value::as_str).filter(|c| !c.is_empty()) {
            progress.session.context = Some(context.to_string());
        }
        if let Some(task) = task.and_then(Value::as_str).filter(|t| !t.is_empty())
            && let Ok(mut current) = self.task.lock()
        {
            *current = Some(task.to_string());
        }
        // The task stays out of the session unless it waits for input.
        self.report_session(progress);
    }

    fn report_session(&self, progress: &mut Progress) {
        let id = progress.session.id();
        if id.is_some() && id != progress.reported {
            progress.reported = id.clone();
            self.out.send(RunEvent::Session { id: id.unwrap_or_default() });
        }
    }

    fn artifact(&self, artifact: &Value, append: bool, last: bool, progress: &mut Progress) {
        let id = artifact.get("artifactId").and_then(Value::as_str).unwrap_or("").to_string();
        let text = text_of(artifact);
        match progress.open.iter_mut().find(|(open, _)| *open == id) {
            Some((_, so_far)) if append => so_far.push_str(&text),
            Some((_, so_far)) => *so_far = text,
            None => progress.open.push_back((id.clone(), text)),
        }
        if last && let Some(i) = progress.open.iter().position(|(open, _)| *open == id) {
            let (_, text) = progress.open.remove(i).unwrap_or_default();
            self.block(text, progress);
        }
    }

    fn flush(&self, progress: &mut Progress) {
        while let Some((_, text)) = progress.open.pop_front() {
            self.block(text, progress);
        }
    }

    fn block(&self, text: String, progress: &mut Progress) {
        let text = text.trim().to_string();
        if text.is_empty() || progress.blocks.last() == Some(&text) {
            return;
        }
        progress.blocks.push(text.clone());
        self.out.send(RunEvent::Text { text });
    }

    fn status(&self, status: &Value, progress: &mut Progress) {
        let state = status.get("state").and_then(Value::as_str).unwrap_or("");
        let text = status.get("message").map(text_of).filter(|t| !t.trim().is_empty());
        match normalise(state).as_str() {
            "COMPLETED" => {
                self.flush(progress);
                if progress.blocks.is_empty()
                    && let Some(text) = text.or(progress.status_text.take())
                {
                    self.block(text, progress);
                }
                progress.end = Some(End::Completed);
            }
            "INPUT_REQUIRED" | "AUTH_REQUIRED" => {
                self.flush(progress);
                let question = match (normalise(state).as_str(), text) {
                    ("AUTH_REQUIRED", text) => {
                        let text = text.unwrap_or_else(|| "The agent wants you to sign in.".into());
                        format!("{text}\n\nmeerora cannot sign in to external agents yet.")
                    }
                    (_, Some(text)) => text,
                    (_, None) => "The agent needs more from you to go on.".into(),
                };
                self.block(question, progress);
                progress.session.task = self.task.lock().ok().and_then(|t| t.clone());
                self.report_session(progress);
                progress.end = Some(End::Waiting);
            }
            "FAILED" | "REJECTED" => {
                self.flush(progress);
                let why = text.unwrap_or_else(|| format!("the task was {}", normalise(state).to_lowercase()));
                progress.end = Some(End::Failed(format!("{} could not answer: {why}", self.host())));
            }
            "CANCELED" | "CANCELLED" => progress.end = Some(End::Cancelled),
            _ => {
                if text.is_some() {
                    progress.status_text = text;
                }
            }
        }
    }

    fn finish(&self, mut progress: Progress) -> Result<String, String> {
        self.flush(&mut progress);
        if progress.end != Some(End::Waiting) {
            progress.session.task = None;
        }
        self.report_session(&mut progress);
        match progress.end {
            Some(End::Failed(e)) => Err(e),
            Some(End::Cancelled) if self.cancelled.load(Ordering::SeqCst) => Err("cancelled".into()),
            Some(End::Cancelled) => Err(format!("{} cancelled the task.", self.host())),
            _ => {
                if progress.blocks.is_empty()
                    && let Some(text) = progress.status_text.take()
                {
                    self.block(text, &mut progress);
                }
                if progress.blocks.is_empty() {
                    Err(format!("{} ended without an answer.", self.host()))
                } else {
                    Ok(progress.blocks.join("\n\n"))
                }
            }
        }
    }
}

/// `TASK_STATE_INPUT_REQUIRED` and `input-required` both give `INPUT_REQUIRED`.
fn normalise(state: &str) -> String {
    state.trim().trim_start_matches("TASK_STATE_").replace('-', "_").to_ascii_uppercase()
}

/// The text parts of a message or artifact, joined.
fn text_of(value: &Value) -> String {
    value
        .get("parts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|p| p.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("")
}

/// Whether the saved agent card offers streaming. None without a card.
fn card_streams(dir: &Path) -> Option<bool> {
    let text = std::fs::read_to_string(dir.join(CARD_FILE)).ok()?;
    let card: Value = serde_json::from_str(&text).ok()?;
    Some(card.get("capabilities").and_then(|c| c.get("streaming")).and_then(Value::as_bool) == Some(true))
}

fn rpc(method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": next_id("rpc"), "method": method, "params": params })
}

fn next_id(prefix: &str) -> String {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis());
    format!("meerora-{prefix}-{ms:x}-{}", NEXT.fetch_add(1, Ordering::Relaxed))
}

fn http(first_byte: Duration) -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_connect(Some(Duration::from_secs(10)))
        .timeout_recv_response(Some(first_byte))
        .timeout_recv_body(Some(Duration::from_secs(600)))
        .build();
    ureq::Agent::new_with_config(config)
}

fn transport_error(host: &str, error: &ureq::Error) -> String {
    match error {
        ureq::Error::ConnectionFailed | ureq::Error::HostNotFound => format!("cannot reach {host}"),
        ureq::Error::Io(e) if matches!(e.kind(), std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::ConnectionReset) => {
            format!("cannot reach {host}")
        }
        ureq::Error::Timeout(_) => format!("{host} did not answer in time"),
        other => format!("cannot reach {host} ({other})"),
    }
}

fn first_line(text: &str, max: usize) -> String {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    line.chars().take(max).collect()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::mpsc::RecvTimeoutError;

    use meerora_index::Demo;
    use meerora_protocol::RuntimeKind;

    use super::*;

    fn runtime(url: &str) -> External {
        External::new(&RuntimeConfig { kind: RuntimeKind::External, model: None, url: Some(url.into()) })
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("meerora-external-{name}-{}-{}", std::process::id(), next_id("t")));
        std::fs::create_dir_all(dir.join("config")).unwrap();
        dir
    }

    /// Runs one turn and returns its events, `Finished` last.
    fn turn(rt: &External, dir: &Path, prompt: &str, session: Option<String>) -> Vec<RunEvent> {
        let request = RunRequest {
            agent: "test".into(),
            dir: dir.to_path_buf(),
            prompt: prompt.into(),
            system: "SECRET SYSTEM PROMPT".into(),
            session,
            ..RunRequest::default()
        };
        let handle = rt.start(request).unwrap();
        let mut events = Vec::new();
        loop {
            match handle.events.recv_timeout(Duration::from_secs(10)) {
                Ok(e) => {
                    let last = matches!(e, RunEvent::Finished { .. });
                    events.push(e);
                    if last {
                        return events;
                    }
                }
                Err(RecvTimeoutError::Timeout) => panic!("no Finished after {events:?}"),
                Err(RecvTimeoutError::Disconnected) => panic!("the run stopped without Finished: {events:?}"),
            }
        }
    }

    fn session_of(events: &[RunEvent]) -> Option<String> {
        events.iter().rev().find_map(|e| if let RunEvent::Session { id } = e { Some(id.clone()) } else { None })
    }

    fn texts(events: &[RunEvent]) -> Vec<String> {
        events.iter().filter_map(|e| if let RunEvent::Text { text } = e { Some(text.clone()) } else { None }).collect()
    }

    fn result(events: &[RunEvent]) -> Result<String, String> {
        match events.last() {
            Some(RunEvent::Finished { result }) => result.clone(),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn streams_text_and_continues_the_context() {
        let demo = Demo::start().unwrap();
        let rt = runtime(&format!("{}/a2a/mira", demo.agents.url()));
        let dir = temp_dir("mira");
        let first = turn(&rt, &dir, "What do seats cost?", None);
        let blocks = texts(&first);
        assert_eq!(blocks.len(), 2, "two paragraphs, each streamed in chunks: {first:?}");
        assert!(blocks[0].starts_with("Business Plus covers teams from 20 seats"), "{blocks:?}");
        assert_eq!(result(&first), Ok(blocks.join("\n\n")));
        assert!(matches!(first[0], RunEvent::Session { .. }), "the context comes first: {first:?}");
        let context = session_of(&first).expect("a context id");
        assert!(!context.starts_with('{'), "no task waits: {context}");

        let second = turn(&rt, &dir, "and for 60?", Some(context.clone()));
        let answer = result(&second).unwrap();
        assert!(answer.contains("For 60 seats on Business Plus that is 8870 euros a year"), "the agent remembered the topic: {answer}");
        assert_eq!(session_of(&second).as_deref(), Some(context.as_str()));

        // A new conversation does not know the topic.
        let fresh = turn(&rt, &dir, "and for 60?", None);
        assert!(!result(&fresh).unwrap().contains("60 seats"));
        assert_ne!(session_of(&fresh), Some(context));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn input_required_asks_the_user_and_the_answer_continues_the_task() {
        let demo = Demo::start().unwrap();
        let rt = runtime(&format!("{}/a2a/tess", demo.agents.url()));
        let dir = temp_dir("tess");
        let ask = turn(&rt, &dir, "I need a ticket", None);
        assert_eq!(result(&ask), Ok("Gladly. Which crossing would you like, and on which day?".into()));
        let waiting = Session::parse(session_of(&ask).as_deref());
        assert!(waiting.context.is_some() && waiting.task.is_some(), "the session holds the waiting task: {waiting:?}");

        let done = turn(&rt, &dir, "Harbour to island on Saturday", session_of(&ask));
        assert!(result(&done).unwrap().starts_with("Done. One ticket for Harbour to island on Saturday is reserved"), "{done:?}");
        let after = Session::parse(session_of(&done).as_deref());
        assert_eq!((after.context, after.task), (waiting.context, None), "same conversation, the task is done");

        // A task the agent no longer knows starts a new one in the same conversation.
        let stale = Session { context: Session::parse(session_of(&done).as_deref()).context, task: Some("task-gone".into()) };
        let retried = turn(&rt, &dir, "When does the next ferry leave?", stale.id());
        assert!(result(&retried).unwrap().contains("every hour"), "{retried:?}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn falls_back_to_send_message_without_streaming() {
        let demo = Demo::start().unwrap();
        let rt = runtime(&format!("{}/a2a/alex", demo.agents.url()));
        // Without a saved card the streaming call is refused and the run retries.
        let dir = temp_dir("alex");
        let events = turn(&rt, &dir, "What does Alex work on?", None);
        assert!(result(&events).unwrap().starts_with("Alex writes software"), "{events:?}");
        // With the card the run does not try to stream at all.
        let card =
            ureq::get(&format!("{}/a2a/alex/agent-card.json", demo.agents.url())).call().unwrap().body_mut().read_to_string().unwrap();
        std::fs::write(dir.join(CARD_FILE), card).unwrap();
        assert_eq!(card_streams(&dir), Some(false));
        let events = turn(&rt, &dir, "How can I reach Alex?", session_of(&events));
        assert!(result(&events).unwrap().contains("pass a note"), "{events:?}");
        let events = turn(&rt, &dir, "Tell Alex I liked the talk.", session_of(&events));
        assert_eq!(result(&events), Ok("Thanks, I will pass that on to Alex.".into()), "the context carried the note topic");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn only_the_prompt_leaves_the_machine() {
        let seen = Arc::new(Mutex::new(Vec::<(String, Value)>::new()));
        let log = seen.clone();
        let server = meerora_index::http::Server::start(
            "127.0.0.1:0",
            Arc::new(move |req: meerora_index::http::Request| {
                let body: Value = serde_json::from_slice(&req.body).unwrap_or_default();
                log.lock().unwrap().push((req.header("a2a-version").unwrap_or("").to_string(), body.clone()));
                meerora_index::http::Response::json(
                    200,
                    &json!({ "jsonrpc": "2.0", "id": body["id"], "result": { "message": {
                        "messageId": "m1", "contextId": "c1", "role": "ROLE_AGENT", "parts": [{ "text": "Noted." }] } } }),
                )
            }),
        )
        .unwrap();
        let dir = temp_dir("privacy");
        let events = turn(&runtime(&format!("{}/a2a", server.url())), &dir, "Hello there", None);
        assert_eq!(result(&events), Ok("Noted.".into()));
        assert_eq!(session_of(&events).as_deref(), Some("c1"));
        let seen = seen.lock().unwrap();
        let (version, body) = &seen[0];
        assert_eq!(version, "1.0");
        assert_eq!(body["method"], "SendStreamingMessage");
        assert_eq!(body["params"]["message"]["role"], "ROLE_USER");
        assert_eq!(body["params"]["message"]["parts"], json!([{ "text": "Hello there" }]));
        let raw = body.to_string();
        assert!(!raw.contains("SECRET") && !raw.contains(&dir.display().to_string()), "{raw}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn errors_are_readable() {
        let dir = temp_dir("errors");
        let none = External::new(&RuntimeConfig { kind: RuntimeKind::External, model: None, url: None });
        assert!(result(&turn(&none, &dir, "hi", None)).unwrap_err().contains("no endpoint"));
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
        let err = result(&turn(&runtime(&format!("http://{closed}/a2a")), &dir, "hi", None)).unwrap_err();
        assert!(err.contains("cannot reach 127.0.0.1"), "{err}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// An endpoint that streams what `events` writes.
    fn streaming(events: fn(&mut meerora_index::http::EventSink) -> std::io::Result<()>) -> meerora_index::http::Server {
        meerora_index::http::Server::start(
            "127.0.0.1:0",
            Arc::new(move |_req: meerora_index::http::Request| meerora_index::http::Response::events(events)),
        )
        .unwrap()
    }

    fn update(artifact: &str, text: &str, append: bool) -> String {
        json!({ "jsonrpc": "2.0", "id": 1, "result": { "artifactUpdate": {
            "taskId": "t", "contextId": "c", "append": append, "lastChunk": false,
            "artifact": { "artifactId": artifact, "parts": [{ "text": text }] } } } })
        .to_string()
    }

    #[test]
    fn hostile_streams_end_with_a_readable_error() {
        let dir = temp_dir("hostile");
        // One line of 2 MiB.
        let server = streaming(|sink| sink.data(&"x".repeat(2 * 1024 * 1024)));
        let err = result(&turn(&runtime(&format!("{}/a2a", server.url())), &dir, "hi", None)).unwrap_err();
        assert!(err.contains("a line is longer than 1 MiB") && err.contains("stopped reading"), "{err}");

        // Artifacts that are opened and never closed.
        let server = streaming(|sink| (0..100).try_for_each(|i| sink.data(&update(&format!("a{i}"), "text", false))));
        let err = result(&turn(&runtime(&format!("{}/a2a", server.url())), &dir, "hi", None)).unwrap_err();
        assert!(err.contains("more than 64 artifacts at once"), "{err}");

        // Endless appends to one artifact.
        let server = streaming(|sink| (0..20).try_for_each(|_| sink.data(&update("a", &"y".repeat(512 * 1024), true))));
        let err = result(&turn(&runtime(&format!("{}/a2a", server.url())), &dir, "hi", None)).unwrap_err();
        assert!(err.contains("more than 4 MiB of text"), "{err}");

        // A stream that never ends, of events that hold nothing.
        let server = streaming(|sink| {
            let working = json!({ "jsonrpc": "2.0", "id": 1, "result": { "statusUpdate": { "taskId": "t", "contextId": "c",
                "status": { "state": "TASK_STATE_WORKING", "message": { "parts": [{ "text": "z".repeat(512 * 1024) }] } } } } })
            .to_string();
            (0..64).try_for_each(|_| sink.data(&working))
        });
        let err = result(&turn(&runtime(&format!("{}/a2a", server.url())), &dir, "hi", None)).unwrap_err();
        assert!(err.contains("more than 16 MiB"), "{err}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn cancel_finishes_at_once() {
        let agents = meerora_index::start_demo_agent("127.0.0.1:0", Duration::from_millis(400)).unwrap();
        let rt = runtime(&format!("{}/a2a/mira", agents.url()));
        let dir = temp_dir("cancel");
        let request = RunRequest { agent: "test".into(), dir: dir.clone(), prompt: "What do seats cost?".into(), ..RunRequest::default() };
        let handle = rt.start(request).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        handle.cancel();
        let mut finished = Vec::new();
        while let Ok(e) = handle.events.recv_timeout(Duration::from_secs(5)) {
            if let RunEvent::Finished { result } = e {
                finished.push(result);
            }
        }
        assert_eq!(finished, [Err("cancelled".to_string())], "exactly one Finished");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn sessions_round_trip() {
        assert_eq!(Session::parse(None), Session::default());
        assert_eq!(Session::parse(Some("ctx-1")), Session { context: Some("ctx-1".into()), task: None });
        let waiting = Session { context: Some("c".into()), task: Some("t".into()) };
        assert_eq!(Session::parse(waiting.id().as_deref()), waiting);
        assert_eq!(Session::default().id(), None);
        assert_eq!(normalise("input-required"), "INPUT_REQUIRED");
        assert_eq!(normalise("TASK_STATE_COMPLETED"), "COMPLETED");
    }
}
