//! A demo A2A 1.0 agent for the demo manifests (Q9), so the whole path from
//! search to chat works without any outside service. It speaks the JSON-RPC
//! binding: `SendMessage`, `SendStreamingMessage` (server-sent events),
//! `GetTask` and `CancelTask`, and serves an agent card per agent. Replies
//! are canned and chosen by keywords, no LLM is involved.
//!
//! | Request | Answer |
//! | --- | --- |
//! | `GET /a2a/{slug}/agent-card.json` | the agent card |
//! | `GET /.well-known/agent-card.json` | the card of the first demo agent |
//! | `POST /a2a/{slug}` | JSON-RPC 2.0 |
//!
//! Mira and Tess stream and answer with tasks whose text arrives as
//! artifact chunks. Tess asks back with `TASK_STATE_INPUT_REQUIRED` when you
//! want a ticket. Alex does not stream and answers with a plain message.
//!
//! The agent remembers the last [`MAX_REMEMBERED`] conversations and tasks,
//! older ones are forgotten.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

use crate::demo::{self, DemoAgent};
use crate::http::{Handler, Request, Response};

/// JSON-RPC error codes of A2A 1.0 (section 5.4) and JSON-RPC itself.
pub mod code {
    pub const PARSE_ERROR: i64 = -32700;
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    pub const TASK_NOT_FOUND: i64 = -32001;
    pub const TASK_NOT_CANCELABLE: i64 = -32002;
    pub const UNSUPPORTED_OPERATION: i64 = -32004;
    pub const VERSION_NOT_SUPPORTED: i64 = -32009;
}

/// Conversations and tasks the demo agent keeps, each.
pub const MAX_REMEMBERED: usize = 1000;

pub struct DemoAgents {
    /// `http://host:port` of this server, for the agent cards.
    base: String,
    /// Pause between streamed chunks.
    delay: Duration,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    seq: u64,
    contexts: HashMap<String, Context>,
    tasks: HashMap<String, Task>,
    /// Ids in the order they came, oldest first, to forget the oldest.
    context_order: VecDeque<String>,
    task_order: VecDeque<String>,
}

impl State {
    /// Forgets the oldest conversations and tasks beyond [`MAX_REMEMBERED`].
    fn forget_oldest(&mut self) {
        while self.contexts.len() > MAX_REMEMBERED
            && let Some(id) = self.context_order.pop_front()
        {
            self.contexts.remove(&id);
        }
        while self.tasks.len() > MAX_REMEMBERED
            && let Some(id) = self.task_order.pop_front()
        {
            self.tasks.remove(&id);
        }
    }
}

/// What an agent remembers of one conversation.
#[derive(Clone, Debug, Default)]
struct Context {
    agent: String,
    turns: u32,
    topic: Option<&'static str>,
}

struct Task {
    context: String,
    agent: String,
    state: &'static str,
    /// What the agent asked for while the task waits for input.
    pending: Option<&'static str>,
    artifacts: Vec<Value>,
    status_message: Option<Value>,
}

/// What a persona answers.
#[derive(Clone, Debug, PartialEq)]
enum Reply {
    /// Paragraphs, the task completes.
    Answer(Vec<String>),
    /// A question, the task waits for input.
    Ask(String),
}

struct Turn {
    task: String,
    context: String,
    reply: Reply,
}

type RpcError = (i64, String);

const COMPLETED: &str = "TASK_STATE_COMPLETED";
const INPUT_REQUIRED: &str = "TASK_STATE_INPUT_REQUIRED";
const CANCELED: &str = "TASK_STATE_CANCELED";

impl DemoAgents {
    pub fn new(base: String, delay: Duration) -> Self {
        Self { base, delay, state: Mutex::new(State::default()) }
    }

    pub fn handler(self: &Arc<Self>) -> Handler {
        let agents = self.clone();
        Arc::new(move |request: Request| agents.route(&request))
    }

    fn route(&self, req: &Request) -> Response {
        match (req.method.as_str(), req.segments().as_slice()) {
            ("GET", ["a2a", slug, "agent-card.json"]) => match demo::agent(slug) {
                Some(agent) => Response::json(200, &self.card(agent)),
                None => Response::error(404, format!("no demo agent {slug}")),
            },
            ("GET", [".well-known", "agent-card.json"]) => Response::json(200, &self.card(&demo::AGENTS[0])),
            ("POST", ["a2a", slug]) => match demo::agent(slug) {
                Some(agent) => self.rpc(agent, req),
                None => Response::error(404, format!("no demo agent {slug}")),
            },
            _ => Response::error(404, format!("nothing at {}", req.path)),
        }
    }

    /// The A2A 1.0 agent card.
    pub fn card(&self, a: &DemoAgent) -> Value {
        let skills: Vec<Value> = a
            .skills
            .iter()
            .map(|(id, name, description, tags)| json!({ "id": id, "name": name, "description": description, "tags": tags }))
            .collect();
        json!({
            "name": a.name,
            "description": a.welcome,
            "supportedInterfaces": [{
                "url": format!("{}/a2a/{}", self.base, a.slug),
                "protocolBinding": "JSONRPC",
                "protocolVersion": "1.0",
            }],
            "provider": { "organization": a.owner, "url": format!("https://{}", a.domain) },
            "version": "1.0.0",
            "capabilities": { "streaming": a.streaming, "pushNotifications": false },
            "defaultInputModes": ["text/plain"],
            "defaultOutputModes": ["text/plain"],
            "skills": skills,
        })
    }

    fn rpc(&self, agent: &'static DemoAgent, req: &Request) -> Response {
        let body: Value = match serde_json::from_slice(&req.body) {
            Ok(v) => v,
            Err(e) => return rpc_error(Value::Null, code::PARSE_ERROR, format!("the body is not JSON ({e})")),
        };
        let id = body.get("id").cloned().unwrap_or(Value::Null);
        if body.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
            return rpc_error(id, code::INVALID_REQUEST, "this is not a JSON-RPC 2.0 request");
        }
        // An empty or missing version means 0.3 (section 3.6).
        let version = req.header("a2a-version").or(req.query("A2A-Version")).unwrap_or("");
        if version != "1.0" && version != "1" {
            return rpc_error(
                id,
                code::VERSION_NOT_SUPPORTED,
                format!("this agent speaks A2A 1.0, not {:?}", if version.is_empty() { "0.3" } else { version }),
            );
        }
        let params = body.get("params").cloned().unwrap_or(Value::Null);
        match body.get("method").and_then(Value::as_str).unwrap_or("") {
            "SendMessage" => match self.turn(agent, &params) {
                Ok(turn) if agent.streaming => rpc_result(id, json!({ "task": self.finish(&turn) })),
                Ok(turn) => rpc_result(id, json!({ "message": self.direct(&turn) })),
                Err((c, m)) => rpc_error(id, c, m),
            },
            "SendStreamingMessage" if !agent.streaming => {
                rpc_error(id, code::UNSUPPORTED_OPERATION, format!("{} does not stream, use SendMessage", agent.name))
            }
            "SendStreamingMessage" => match self.turn(agent, &params) {
                Ok(turn) => self.stream(id, turn),
                Err((c, m)) => rpc_error(id, c, m),
            },
            "GetTask" => match self.task_json(params.get("id").and_then(Value::as_str).unwrap_or("")) {
                Some(task) => rpc_result(id, task),
                None => rpc_error(id, code::TASK_NOT_FOUND, "Task not found"),
            },
            "CancelTask" => self.cancel(id, params.get("id").and_then(Value::as_str).unwrap_or("")),
            other => rpc_error(id, code::METHOD_NOT_FOUND, format!("{other:?} is not a method of this agent")),
        }
    }

    fn next_id(&self, state: &mut State, prefix: &str) -> String {
        state.seq += 1;
        format!("{prefix}-{:x}-{}", crate::now_ms(), state.seq)
    }

    /// Runs one user message through the persona and records the task.
    fn turn(&self, agent: &'static DemoAgent, params: &Value) -> Result<Turn, RpcError> {
        let message = params.get("message").ok_or((code::INVALID_PARAMS, "params.message is missing".to_string()))?;
        let role = message.get("role").and_then(Value::as_str).unwrap_or("");
        if role != "ROLE_USER" && role != "user" {
            return Err((code::INVALID_PARAMS, format!("the message role must be ROLE_USER, not {role:?}")));
        }
        let text = text_of(message);
        if text.trim().is_empty() {
            return Err((code::INVALID_PARAMS, "the message has no text part".into()));
        }
        let mut state = self.state.lock().map_err(|_| (-32603, "the agent's state is poisoned".to_string()))?;
        let mut context = message.get("contextId").and_then(Value::as_str).map(str::to_string);
        let mut pending = None;
        let task = match message.get("taskId").and_then(Value::as_str) {
            Some(task_id) => {
                let task = state.tasks.get(task_id).ok_or((code::TASK_NOT_FOUND, "Task not found".to_string()))?;
                if context.as_ref().is_some_and(|c| *c != task.context) {
                    return Err((code::INVALID_PARAMS, "contextId and taskId do not match".into()));
                }
                if task.state != INPUT_REQUIRED {
                    return Err((code::INVALID_PARAMS, format!("task {task_id} is in a terminal state")));
                }
                context = Some(task.context.clone());
                pending = task.pending;
                task_id.to_string()
            }
            None => self.next_id(&mut state, "task"),
        };
        let context = match context {
            Some(c) => c,
            None => self.next_id(&mut state, "ctx"),
        };
        if !state.contexts.contains_key(&context) {
            state.context_order.push_back(context.clone());
        }
        let ctx = state.contexts.entry(context.clone()).or_insert_with(|| Context { agent: agent.slug.into(), ..Context::default() });
        if ctx.agent != agent.slug {
            return Err((code::INVALID_PARAMS, format!("context {context} belongs to another agent")));
        }
        let (reply, next_pending) = persona(agent.slug, &text, ctx, pending);
        ctx.turns += 1;
        let (state_name, status_message) = match &reply {
            Reply::Answer(_) => (COMPLETED, None),
            Reply::Ask(question) => (INPUT_REQUIRED, Some(agent_message(&context, Some(&task), question))),
        };
        let artifacts = match &reply {
            Reply::Answer(paragraphs) => paragraphs.iter().enumerate().map(|(i, p)| artifact(&task, i, p)).collect(),
            Reply::Ask(_) => Vec::new(),
        };
        let new = state
            .tasks
            .insert(
                task.clone(),
                Task {
                    context: context.clone(),
                    agent: agent.slug.into(),
                    state: state_name,
                    pending: next_pending,
                    artifacts,
                    status_message,
                },
            )
            .is_none();
        if new {
            state.task_order.push_back(task.clone());
        }
        state.forget_oldest();
        Ok(Turn { task, context, reply })
    }

    /// The finished task, the answer of `SendMessage`.
    fn finish(&self, turn: &Turn) -> Value {
        self.task_json(&turn.task).unwrap_or(Value::Null)
    }

    /// A direct message, the answer of agents that need no task.
    fn direct(&self, turn: &Turn) -> Value {
        let text = match &turn.reply {
            Reply::Answer(paragraphs) => paragraphs.join("\n\n"),
            Reply::Ask(question) => question.clone(),
        };
        agent_message(&turn.context, None, &text)
    }

    fn task_json(&self, id: &str) -> Option<Value> {
        let state = self.state.lock().ok()?;
        let task = state.tasks.get(id)?;
        let mut status = json!({ "state": task.state });
        if let Some(message) = &task.status_message {
            status["message"] = message.clone();
        }
        Some(
            json!({ "id": id, "contextId": task.context, "status": status, "artifacts": task.artifacts, "metadata": { "agent": task.agent } }),
        )
    }

    fn cancel(&self, id: Value, task_id: &str) -> Response {
        {
            let Ok(mut state) = self.state.lock() else { return rpc_error(id, -32603, "the agent's state is poisoned") };
            let Some(task) = state.tasks.get_mut(task_id) else { return rpc_error(id, code::TASK_NOT_FOUND, "Task not found") };
            if task.state != INPUT_REQUIRED {
                return rpc_error(id, code::TASK_NOT_CANCELABLE, "the task already finished");
            }
            task.state = CANCELED;
            task.pending = None;
        }
        rpc_result(id, self.task_json(task_id).unwrap_or(Value::Null))
    }

    /// `SendStreamingMessage`: the task first, then a working status, the
    /// answer as artifact chunks and the final status (section 3.1.2).
    fn stream(&self, id: Value, turn: Turn) -> Response {
        let delay = self.delay;
        let Turn { task, context, reply } = turn;
        Response::events(move |sink| {
            let mut send = |result: Value| -> io::Result<()> {
                sink.data(&json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string())?;
                std::thread::sleep(delay);
                Ok(())
            };
            send(json!({ "task": { "id": task, "contextId": context, "status": { "state": "TASK_STATE_SUBMITTED" } } }))?;
            send(status_update(&task, &context, json!({ "state": "TASK_STATE_WORKING" })))?;
            match reply {
                Reply::Answer(paragraphs) => {
                    for (i, paragraph) in paragraphs.iter().enumerate() {
                        let chunks = chunks(paragraph, 3);
                        let last = chunks.len() - 1;
                        for (j, chunk) in chunks.into_iter().enumerate() {
                            let mut piece = artifact(&task, i, &chunk);
                            if j > 0 {
                                // Appended chunks carry only the new text.
                                piece.as_object_mut().map(|o| o.remove("name"));
                            }
                            send(json!({ "artifactUpdate": {
                                "taskId": task, "contextId": context, "artifact": piece, "append": j > 0, "lastChunk": j == last,
                            } }))?;
                        }
                    }
                    send(status_update(&task, &context, json!({ "state": COMPLETED })))
                }
                Reply::Ask(question) => send(status_update(
                    &task,
                    &context,
                    json!({ "state": INPUT_REQUIRED, "message": agent_message(&context, Some(&task), &question) }),
                )),
            }
        })
    }
}

fn status_update(task: &str, context: &str, status: Value) -> Value {
    json!({ "statusUpdate": { "taskId": task, "contextId": context, "status": status } })
}

fn artifact(task: &str, index: usize, text: &str) -> Value {
    json!({ "artifactId": format!("{task}-{index}"), "name": "answer", "parts": [{ "text": text, "mediaType": "text/plain" }] })
}

pub(crate) fn agent_message(context: &str, task: Option<&str>, text: &str) -> Value {
    let mut m = json!({
        "messageId": format!("msg-{:x}-{}", crate::now_ms(), text.len()),
        "contextId": context,
        "role": "ROLE_AGENT",
        "parts": [{ "text": text }],
    });
    if let Some(task) = task {
        m["taskId"] = json!(task);
    }
    m
}

/// The text parts of a message, joined by line breaks.
pub fn text_of(message: &Value) -> String {
    message
        .get("parts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|p| p.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
}

pub(crate) fn rpc_result(id: Value, result: Value) -> Response {
    Response::json(200, &json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

pub(crate) fn rpc_error(id: Value, code: i64, message: impl Into<String>) -> Response {
    Response::json(200, &json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message.into() } }))
}

/// Splits `text` into at most `n` pieces at word starts. The pieces join
/// back into `text`.
fn chunks(text: &str, n: usize) -> Vec<String> {
    let starts: Vec<usize> = text.match_indices(' ').map(|(i, _)| i + 1).collect();
    if starts.is_empty() || n < 2 {
        return vec![text.to_string()];
    }
    let step = (starts.len() / n).max(1);
    let mut cuts: Vec<usize> = starts.iter().copied().skip(step - 1).step_by(step).take(n - 1).collect();
    cuts.dedup();
    let mut pieces = Vec::new();
    let mut from = 0;
    for cut in cuts {
        pieces.push(text[from..cut].to_string());
        from = cut;
    }
    pieces.push(text[from..].to_string());
    pieces
}

/// The canned replies of the demo agents.
fn persona(slug: &str, text: &str, ctx: &mut Context, pending: Option<&'static str>) -> (Reply, Option<&'static str>) {
    let words = crate::search::words(text);
    let prefix = |keys: &[&str]| words.iter().any(|w| keys.iter().any(|k| w.starts_with(k)));
    let exact = |keys: &[&str]| words.iter().any(|w| keys.contains(&w.as_str()));
    let number = text.split(|c: char| !c.is_ascii_digit()).find(|s| !s.is_empty()).and_then(|s| s.parse::<u32>().ok());
    let greeting = exact(&["hello", "hi", "hey", "hallo", "moin", "ahoy"]);
    let answer = |paragraphs: &[&str]| Reply::Answer(paragraphs.iter().map(|p| p.to_string()).collect());
    let welcome = demo::agent(slug).map_or("Hello.", |a| a.welcome);
    match slug {
        "mira" => {
            let quote = |n: u32| {
                Reply::Answer(vec![
                    format!(
                        "Thanks. Your formal quote for {n} seats on Business Plus is ready, {} euros a year{}.",
                        price(n),
                        discount_note(n)
                    ),
                    "It is signed by Meridian and valid for 30 days. Tell me if anything should change.".into(),
                ])
            };
            if pending == Some("quote") || prefix(&["quote", "offer"]) {
                ctx.topic = Some("a formal quote");
                return match number {
                    Some(n) => (quote(n), None),
                    None if pending == Some("quote") => {
                        (Reply::Ask("I need a number for the quote. How many seats should it cover?".into()), Some("quote"))
                    }
                    None => (Reply::Ask("Happy to prepare a formal quote. How many seats do you need?".into()), Some("quote")),
                };
            }
            if prefix(&["price", "pricing", "cost", "seat", "plan", "licen"]) || (number.is_some() && ctx.topic == Some("seat prices")) {
                ctx.topic = Some("seat prices");
                return match number {
                    Some(n) => (
                        Reply::Answer(vec![
                            format!("For {n} seats on Business Plus that is {} euros a year{}.", price(n), discount_note(n)),
                            "Shall I prepare a formal quote?".into(),
                        ]),
                        None,
                    ),
                    None => (
                        answer(&[
                            "Business Plus covers teams from 20 seats at 14 euros per seat and month, billed yearly.",
                            "From 50 seats a volume discount takes 12 percent off. Ask me for a formal quote whenever you like.",
                        ]),
                        None,
                    ),
                };
            }
            if exact(&["eu", "gdpr", "where"]) || prefix(&["residen", "data", "privacy", "frankfurt", "dublin"]) {
                ctx.topic = Some("data residency");
                return (
                    answer(&[
                        "Customer data stays in EU regions by default, on every plan.",
                        "Business Plus adds a signed data processing agreement and the choice between Frankfurt and Dublin.",
                    ]),
                    None,
                );
            }
            if prefix(&["discount", "volume", "cheap"]) {
                ctx.topic = Some("seat prices");
                return (
                    answer(&[
                        "From 50 seats the volume discount takes 12 percent off the list price.",
                        "From 200 seats we work out a custom agreement together.",
                    ]),
                    None,
                );
            }
            if greeting {
                return (answer(&[welcome]), None);
            }
            match ctx.topic {
                Some(topic) => (
                    Reply::Answer(vec![format!(
                        "We were talking about {topic}. I can also tell you about data residency or volume discounts."
                    )]),
                    None,
                ),
                None => (
                    answer(&[
                        "I can help with Meridian's plans, seat pricing, EU data residency and volume discounts.",
                        "What would you like to know?",
                    ]),
                    None,
                ),
            }
        }
        "tess" => {
            if pending == Some("ticket") {
                let crossing = text.trim().trim_end_matches(['.', '!', '?']);
                return (
                    Reply::Answer(vec![
                        format!("Done. One ticket for {crossing} is reserved under your name."),
                        "Bikes ride free on every crossing. Show the code at the gate.".into(),
                    ]),
                    None,
                );
            }
            if prefix(&["ticket", "book", "reserv"]) {
                return (Reply::Ask("Gladly. Which crossing would you like, and on which day?".into()), Some("ticket"));
            }
            if prefix(&["timetable", "time", "when", "depart", "leave", "schedule", "next"]) {
                return (
                    answer(&[
                        "Ferries leave the harbour every hour from 6:00 to 22:00.",
                        "The last boat back from the island leaves at 21:30.",
                    ]),
                    None,
                );
            }
            if prefix(&["bike", "bicycle", "cycl"]) {
                return (
                    answer(&["Bikes ride free on every crossing.", "Leave them on the lower deck, the crew helps with the straps."]),
                    None,
                );
            }
            if greeting {
                return (answer(&[welcome]), None);
            }
            (answer(&["I can help with timetables, tickets and bikes on board.", "What would you like to know?"]), None)
        }
        _ => {
            if ctx.topic == Some("note") {
                ctx.topic = None;
                return (answer(&["Thanks, I will pass that on to Alex."]), None);
            }
            if prefix(&["contact", "reach", "mail", "meet", "hire", "note", "message"]) {
                ctx.topic = Some("note");
                return (answer(&["I can pass a note to Alex. Tell me what it is about and how Alex can reach you."]), None);
            }
            if prefix(&["work", "job", "project", "build"]) || exact(&["do"]) {
                return (
                    answer(&[
                        "Alex writes software for small teams and works freelance from Example City. Right now Alex builds tools for local first apps.",
                    ]),
                    None,
                );
            }
            if prefix(&["sail", "boat", "weekend", "hobb"]) {
                return (answer(&["Alex sails on weekends, mostly on the lake near Example City."]), None);
            }
            if greeting {
                return (answer(&[welcome]), None);
            }
            (answer(&["I can tell you what Alex works on and what Alex likes to do, or pass Alex a note."]), None)
        }
    }
}

/// Euros a year on Business Plus, with the volume discount from 50 seats.
fn price(seats: u32) -> u64 {
    let list = u64::from(seats) * 14 * 12;
    if seats >= 50 { list * 88 / 100 } else { list }
}

fn discount_note(seats: u32) -> &'static str {
    if seats >= 50 { " after the volume discount" } else { "" }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> Context {
        Context { agent: "mira".into(), ..Context::default() }
    }

    #[test]
    fn the_demo_agent_forgets_old_conversations() {
        let agents = DemoAgents::new("http://127.0.0.1:1".into(), Duration::ZERO);
        let message = json!({ "message": { "role": "ROLE_USER", "parts": [{ "text": "hi" }] } });
        let mira = demo::agent("mira").unwrap();
        let first = agents.turn(mira, &message).unwrap();
        for _ in 0..MAX_REMEMBERED + 50 {
            agents.turn(mira, &message).unwrap();
        }
        let state = agents.state.lock().unwrap();
        assert_eq!((state.contexts.len(), state.tasks.len()), (MAX_REMEMBERED, MAX_REMEMBERED));
        assert_eq!((state.context_order.len(), state.task_order.len()), (MAX_REMEMBERED, MAX_REMEMBERED));
        assert!(!state.tasks.contains_key(&first.task) && !state.contexts.contains_key(&first.context), "the oldest are gone");
    }

    #[test]
    fn chunks_join_back() {
        let text = "Customer data stays in EU regions by default, on every plan.";
        let pieces = chunks(text, 3);
        assert_eq!(pieces.len(), 3);
        assert_eq!(pieces.concat(), text);
        assert_eq!(chunks("one", 3), ["one"]);
        assert_eq!(chunks("one two", 3).concat(), "one two");
    }

    #[test]
    fn mira_remembers_the_topic() {
        let mut c = ctx();
        let (first, _) = persona("mira", "What do seats cost?", &mut c, None);
        assert!(matches!(&first, Reply::Answer(p) if p[0].contains("14 euros")));
        let (second, _) = persona("mira", "and for 60?", &mut c, None);
        assert!(matches!(&second, Reply::Answer(p) if p[0].contains("60 seats") && p[0].contains("8870")), "{second:?}");
        let (fresh, _) = persona("mira", "and for 60?", &mut ctx(), None);
        assert!(matches!(&fresh, Reply::Answer(p) if !p[0].contains("60 seats")), "{fresh:?}");
    }

    #[test]
    fn tess_asks_for_the_crossing() {
        let mut c = Context { agent: "tess".into(), ..Context::default() };
        let (ask, pending) = persona("tess", "I need a ticket", &mut c, None);
        assert_eq!((ask, pending), (Reply::Ask("Gladly. Which crossing would you like, and on which day?".into()), Some("ticket")));
        let (done, pending) = persona("tess", "Harbour to island on Saturday.", &mut c, Some("ticket"));
        assert!(
            matches!(&done, Reply::Answer(p) if p[0] == "Done. One ticket for Harbour to island on Saturday is reserved under your name.")
        );
        assert_eq!(pending, None);
    }
}
