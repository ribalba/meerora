//! One-shot questions from the command box of the app (`?? question`,
//! [`Command::Ask`]). The daemon asks the agent's runtime once, outside of
//! its runs: no conversation, no session, no meerora tools, read-only
//! permissions and a private working folder, with the CLI home and the
//! sandbox of the agent's runs (D16, D29, Q15). The answer goes back to the
//! asking client only, as [`Event::Answer`] that grows with every text
//! block. Nothing is stored, the figure does not move and the ring stays.
//!
//! [`Command::Ask`]: meerora_protocol::Command::Ask

use std::collections::BTreeMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

use meerora_protocol::{Event, RuntimeConfig, RuntimeKind, Settings};
use meerora_runtime::{Isolation, Permissions, RunEvent, RunRequest, Runtime};
use tracing::{info, warn};

use crate::daemon::{Daemon, Msg, Outgoing, RuntimeFactory};
use crate::runs::panic_text;

/// The longest a one-shot answer may take.
pub const ANSWER_TIMEOUT: Duration = Duration::from_secs(120);
/// One-shot questions that may run at the same time. The app asks one at a time.
const MAX_RUNNING: usize = 4;
/// The longest question the daemon passes on.
const MAX_QUESTION: usize = 4000;

const SYSTEM: &str = "You answer one quick question that the user typed into the command box of meerora, \
a desktop where LLM agents live. Answer at once and briefly: a few plain sentences or a short list, \
no headings, no follow-up questions. Do not use tools. If the question needs files, the web or a longer \
conversation, say so in one sentence.";

/// The one-shot questions that are running, by request id, with their stop flags.
#[derive(Default)]
pub struct Asks {
    running: BTreeMap<String, Arc<AtomicBool>>,
}

/// One question for [`answer`].
pub struct Job {
    pub client: u64,
    pub request: String,
    pub agent: String,
    pub question: String,
    pub config: RuntimeConfig,
    pub isolation: Option<Isolation>,
}

impl Daemon {
    /// [`Command::Ask`](meerora_protocol::Command::Ask) from `client`.
    pub(crate) fn ask(&mut self, client: u64, request: String, agent: &str, question: String) {
        let refuse = |daemon: &mut Daemon, error: String| {
            daemon.out.push(Outgoing::To(
                client,
                Event::Answer { request: request.clone(), text: String::new(), done: true, error: Some(error) },
            ));
        };
        let question = question.trim().to_string();
        if question.is_empty() {
            return refuse(self, "Type a question after ??.".into());
        }
        if question.chars().count() > MAX_QUESTION {
            return refuse(self, format!("The question is longer than {MAX_QUESTION} characters."));
        }
        let Some(dir) = self.agents.get(agent).map(|a| a.dir.clone()) else {
            return refuse(self, format!("There is no agent called {agent}."));
        };
        let name = dir.config.name.clone();
        let config = match dir.config.runtime.clone() {
            Some(config) if config.kind != RuntimeKind::Mock => config,
            _ => return refuse(self, format!("{name} is a mock agent that plays the demo scenario. Give it a runtime in its agent.toml.")),
        };
        if self.asks.running.contains_key(&request) {
            return refuse(self, "This question is already being answered.".into());
        }
        if self.asks.running.len() >= MAX_RUNNING {
            return refuse(self, "Too many quick questions at once. Wait for an answer first.".into());
        }
        // External agents get only what you typed, and no isolation applies to them (D25).
        let isolation = (config.kind != RuntimeKind::External).then(|| self.isolation(agent));
        let stop = Arc::new(AtomicBool::new(false));
        self.asks.running.insert(request.clone(), stop.clone());
        info!("client {client} asks {agent} a quick question ({request})");
        let job = Job { client, request: request.clone(), agent: agent.to_string(), question, config, isolation };
        let (factory, settings, tx) = (self.opts.factory.clone(), self.opts.settings.clone(), self.tx.clone());
        let spawned = std::thread::Builder::new().name(format!("ask-{agent}")).spawn(move || {
            answer(&job, &factory, &settings, &stop, &mut |event| {
                let _ = tx.send(Msg::Answer { client: job.client, event });
            });
        });
        if let Err(e) = spawned {
            warn!("cannot start a thread for a quick question ({e})");
            self.asks.running.remove(&request);
            refuse(self, format!("meerora cannot start the question ({e})."));
        }
    }

    /// [`Command::CancelAsk`](meerora_protocol::Command::CancelAsk). The
    /// question ends with an answer that says it was stopped.
    pub(crate) fn cancel_ask(&mut self, request: &str) {
        if let Some(stop) = self.asks.running.get(request) {
            info!("quick question {request} stopped");
            stop.store(true, Ordering::SeqCst);
        }
    }

    /// An answer from a question's thread, for the client that asked.
    pub(crate) fn on_answer(&mut self, client: u64, event: Event) {
        if let Event::Answer { request, done: true, .. } = &event {
            self.asks.running.remove(request);
        }
        self.out.push(Outgoing::To(client, event));
    }
}

/// Asks the runtime and reports the growing answer through `emit`. The last
/// event has `done` set, with the error when there is one.
pub fn answer(job: &Job, factory: &RuntimeFactory, settings: &Settings, stop: &AtomicBool, emit: &mut dyn FnMut(Event)) {
    let (text, error) = ask_once(job, factory, settings, stop, emit);
    // The private folder is gone by now, before the client hears the end.
    emit(Event::Answer { request: job.request.clone(), text, done: true, error });
}

/// Runs the question in a private folder and returns the whole answer and
/// the error. Growing answers go out through `emit`.
fn ask_once(
    job: &Job,
    factory: &RuntimeFactory,
    settings: &Settings,
    stop: &AtomicBool,
    emit: &mut dyn FnMut(Event),
) -> (String, Option<String>) {
    let dir = match WorkDir::new() {
        Ok(dir) => dir,
        Err(e) => return (String::new(), Some(format!("meerora cannot create a folder for the question ({e})."))),
    };
    let request = RunRequest {
        agent: job.agent.clone(),
        dir: dir.0.clone(),
        prompt: job.question.clone(),
        system: SYSTEM.into(),
        session: None,
        model: job.config.model.clone(),
        permissions: Permissions { write: false, shell: false, web: false },
        mcp: None,
        isolation: job.isolation.clone(),
    };
    let started = catch_unwind(AssertUnwindSafe(|| -> anyhow::Result<_> {
        let runtime: Box<dyn Runtime> = factory(&job.config, settings)?;
        let handle = runtime.start(request)?;
        Ok((runtime, handle))
    }));
    let (_runtime, handle) = match started {
        Ok(Ok(started)) => started,
        Ok(Err(e)) => return (String::new(), Some(format!("The question could not start ({e:#})."))),
        Err(panic) => return (String::new(), Some(format!("The runtime crashed ({}).", panic_text(&*panic)))),
    };
    let deadline = Instant::now() + ANSWER_TIMEOUT;
    let mut text = String::new();
    let mut stopped = false;
    loop {
        if !stopped && stop.load(Ordering::SeqCst) {
            stopped = true;
            handle.cancel();
        }
        if Instant::now() >= deadline {
            handle.cancel();
            return (text, Some("The answer took longer than 2 minutes.".into()));
        }
        match handle.events.recv_timeout(Duration::from_millis(100)) {
            Ok(RunEvent::Text { text: block }) => {
                let block = block.trim();
                if block.is_empty() {
                    continue;
                }
                if !text.is_empty() {
                    text.push_str("\n\n");
                }
                text.push_str(block);
                emit(Event::Answer { request: job.request.clone(), text: text.clone(), done: false, error: None });
            }
            Ok(RunEvent::Finished { result }) => {
                return match result {
                    _ if stopped => (text, Some("Stopped.".into())),
                    Ok(last) if text.is_empty() => (last.trim().to_string(), None),
                    Ok(_) => (text, None),
                    Err(e) => (text, Some(e)),
                };
            }
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return (text, Some("The runtime stopped without an answer.".into())),
        }
    }
}

/// A private folder for one question, removed again afterwards.
struct WorkDir(PathBuf);

impl WorkDir {
    fn new() -> std::io::Result<Self> {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("meerora-ask-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        Ok(Self(dir))
    }
}

impl Drop for WorkDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
