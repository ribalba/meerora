//! Runs of real agents (D22): how a run starts, how its [`RunEvent`]s become
//! protocol events, and how it ends.

use std::any::Any;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use meerora_protocol::{AgentDir, Event, OnDone, Outcome, RingState, Role, RuntimeConfig, Settings, Trigger};
use meerora_runtime::{Isolation, Permissions, RunEvent, RunRequest, Runtime};
use tracing::{debug, info, warn};

use crate::daemon::{ActiveRun, Daemon, Msg, RuntimeFactory};
use crate::git;
use crate::store::LastRun;

/// Serialises git commits, so two quick runs never fight over the index.
static GIT: Mutex<()> = Mutex::new(());

impl Daemon {
    /// Starts a run of a real agent. `echo` posts the task as the user's message.
    pub(crate) fn start_run(&mut self, id: &str, trigger: Trigger, task: String, station: Option<String>, echo: bool) {
        let Some(runtime) = self.agents.get(id).and_then(|a| a.live.as_ref().and(a.dir.config.runtime.clone())) else {
            return;
        };
        let now = now_ms();
        let run_id = format!("{id}-{now}");
        info!("{id} starts run {run_id} ({})", if trigger == Trigger::User { "from you" } else { "from its schedule" });
        self.emit(Event::RunStarted { agent: id.into(), run: run_id.clone(), trigger, task: task.clone(), started: now });
        self.emit(Event::Ring { agent: id.into(), state: RingState::Working, progress: None });
        self.emit(Event::Activity { agent: id.into(), text: cut(first_line(&task), 60) });
        if echo {
            self.post(id, format!("{run_id}-you"), Role::User, task.clone(), now);
        }
        if let Some(station) = station {
            self.emit(Event::GoTo { agent: id.into(), station });
        }

        // External agents get no meerora tools (D25).
        let mcp = if runtime.kind == meerora_protocol::RuntimeKind::External { None } else { self.bridge(id) };
        let granted = self.claim_capabilities(id, &run_id);
        let isolation = self.isolation(id);
        // The sandbox mounts logs/ read-only only when it exists, so it must
        // exist before the agent could create it itself (meerora-runtime PROTECTED).
        if let Some(agent) = self.agents.get(id)
            && let Err(e) = std::fs::create_dir_all(agent.dir.path.join("logs"))
        {
            warn!("cannot create the logs of {id} ({e})");
        }
        let (factory, settings, tx) = (self.opts.factory.clone(), self.opts.settings.clone(), self.tx.clone());
        let Some(agent) = self.agents.get_mut(id) else { return };
        let Some(live) = agent.live.as_mut() else { return };
        // agent.toml plus the grants that hold once (D8).
        let mut dir = agent.dir.clone();
        dir.config.capabilities.granted = granted.clone();
        let request = RunRequest {
            agent: id.into(),
            dir: agent.dir.path.clone(),
            prompt: task.clone(),
            system: system_prompt_with(&dir, &self.opts.stations, &isolation.workspace),
            session: live.store.session.clone(),
            model: runtime.model.clone(),
            permissions: Permissions::from_capabilities(&dir.config.capabilities),
            mcp,
            isolation: Some(isolation),
        };
        let cancel = Arc::new(AtomicBool::new(false));
        live.run = Some(ActiveRun {
            id: run_id.clone(),
            trigger,
            task,
            started: now,
            reply_id: format!("{run_id}-reply"),
            reply_at: None,
            text: String::new(),
            ask: None,
            cancel: cancel.clone(),
            cancelled: false,
            dropped: 0,
            tokens: 0,
            granted,
            continuation: Vec::new(),
            continue_by: None,
        });
        let spawned = std::thread::Builder::new()
            .name(format!("run-{id}"))
            .spawn(move || forward(factory, runtime, settings, request, run_id, cancel, tx));
        if let Err(e) = spawned {
            warn!("cannot start a thread for the run of {id} ({e})");
            self.finish_run(id, Err(format!("meerora cannot start the run ({e})")));
        }
    }

    /// The CLI home and the sandbox of a run of `id` (D16, Q15, Q16). The
    /// sandbox shows the space, which the MCP bridge reads, and the agent's
    /// bridge socket.
    pub(crate) fn isolation(&self, id: &str) -> Isolation {
        let sandbox = &self.opts.settings.sandbox;
        let mut read = vec![meerora_protocol::space_dir(&self.opts.data_dir)];
        // The script API, so agents can read how to write a device (system_prompt).
        read.extend(script_api(&self.opts.data_dir));
        read.extend(sandbox.read.iter().cloned());
        // Workspace folders from agent.toml, checked again before every run.
        let folders = self.agents.get(id).map(|a| a.dir.config.workspace.clone()).unwrap_or_default();
        let home = meerora_runtime::sandbox::user_home();
        let (workspace, refused) = meerora_protocol::workspace::resolve(&folders, home.as_deref(), &self.opts.data_dir);
        for why in refused {
            warn!("a workspace folder of {id} is left out: {why}");
        }
        // Which .git entries existed before an agent could write there (workspace-trust.json).
        if let Err(e) = meerora_protocol::workspace::record_run_start(&self.opts.data_dir, id, &workspace) {
            warn!("cannot record the workspace repositories of {id} ({e})");
        }
        Isolation {
            runtimes: self.opts.data_dir.join("runtimes"),
            sandbox: sandbox.enabled,
            read,
            sockets: self.bridge_sockets.get(id).cloned().into_iter().collect(),
            workspace,
        }
    }

    pub(crate) fn on_run_event(&mut self, id: &str, run_id: &str, event: RunEvent) {
        let Some(live) = self.agents.get_mut(id).and_then(|a| a.live.as_mut()) else { return };
        let Some(run) = live.run.as_mut().filter(|r| r.id == run_id) else {
            debug!("event of an old run of {id} ignored");
            return;
        };
        match event {
            RunEvent::Session { id: session } => live.store.session = Some(session),
            RunEvent::Text { text } => {
                let text = text.trim();
                if text.is_empty() {
                    return;
                }
                if !run.text.is_empty() {
                    run.text.push_str("\n\n");
                }
                run.text.push_str(text);
                let at = *run.reply_at.get_or_insert_with(now_ms);
                let (message, full) = (run.reply_id.clone(), run.text.clone());
                self.out.push(crate::daemon::Outgoing::All(Event::Message {
                    agent: id.into(),
                    id: message,
                    role: Role::Agent,
                    text: full,
                    done: false,
                    at,
                }));
            }
            // An act or open of the app tools logs its own line, such as
            // `terminal.run cargo test`, once the app did it (app_tools.rs, D30).
            RunEvent::Tool { tool, .. } if matches!(tool.as_str(), "meerora.app_act" | "meerora.app_open") => {}
            RunEvent::Tool { tool, summary, target } => {
                let activity = activity_for(&tool, &summary);
                self.emit(Event::Action { agent: id.into(), tool, summary, target });
                if let Some(text) = activity {
                    self.emit(Event::Activity { agent: id.into(), text });
                }
            }
            RunEvent::Usage { input_tokens, output_tokens } => {
                let tokens = input_tokens + output_tokens;
                run.tokens += tokens;
                live.store.add_tokens(&today(), tokens);
            }
            RunEvent::Finished { result } => self.finish_run(id, result),
        }
    }

    /// Posts the answer, reports the outcome, sets the ring, saves the state,
    /// commits the directory and starts the next queued run. A run that got
    /// a capability while it went is continued by a new run (capabilities.rs).
    pub(crate) fn finish_run(&mut self, id: &str, result: Result<String, String>) {
        let now = now_ms();
        let Some(agent) = self.agents.get_mut(id) else { return };
        let (on_done, home, path) = (agent.dir.config.on_done, agent.dir.config.home.clone(), agent.dir.path.clone());
        let Some(live) = agent.live.as_mut() else { return };
        let Some(run) = live.run.take() else { return };
        let reply_at = run.reply_at.unwrap_or(now);
        let continuing = !run.continuation.is_empty() && !run.cancelled;

        let (outcome, summary) = match &result {
            _ if continuing => {
                let text = match &result {
                    Ok(answer) if run.text.trim().is_empty() => answer.trim().to_string(),
                    _ => run.text.clone(),
                };
                if !text.trim().is_empty() {
                    self.post(id, run.reply_id.clone(), Role::Agent, text, reply_at);
                }
                if let Err(e) = &result {
                    info!("{id} ended its run for the continuation ({e})");
                }
                (Outcome::Done, Some(format!("continues with {}", run.continuation.join(", "))))
            }
            Ok(answer) => {
                let text = if run.text.trim().is_empty() { answer.trim().to_string() } else { run.text.clone() };
                if !text.is_empty() {
                    self.post(id, run.reply_id.clone(), Role::Agent, text.clone(), reply_at);
                }
                let sentence = first_sentence(&text);
                if let Some(question) = &run.ask {
                    (Outcome::NeedsYou, Some(question.clone()))
                } else if run.trigger == Trigger::User && on_done == OnDone::Wait {
                    (Outcome::NeedsYou, sentence)
                } else {
                    (Outcome::Done, sentence)
                }
            }
            Err(error) => {
                if !run.text.trim().is_empty() {
                    self.post(id, run.reply_id.clone(), Role::Agent, run.text.clone(), reply_at);
                }
                if run.cancelled {
                    let note = match run.dropped {
                        0 => "Cancelled.".to_string(),
                        1 => "Cancelled. The message that waited for this run was dropped.".to_string(),
                        n => format!("Cancelled. The {n} messages that waited for this run were dropped."),
                    };
                    self.post_system(id, note);
                    (Outcome::Cancelled, Some("cancelled".to_string()))
                } else {
                    self.post_system(id, error.clone());
                    (Outcome::Failed, Some(cut(first_line(error), 120)))
                }
            }
        };
        info!("{id} finished run {} ({outcome:?})", run.id);
        self.emit(Event::RunFinished { agent: id.into(), run: run.id.clone(), outcome, summary: summary.clone() });
        if continuing {
            // The figure keeps working, the next run starts right away.
        } else if outcome == Outcome::NeedsYou {
            self.emit(Event::Ring { agent: id.into(), state: RingState::NeedsYou, progress: None });
            self.emit(Event::Activity { agent: id.into(), text: cut(summary.as_deref().unwrap_or("needs you"), 60) });
        } else {
            self.emit(Event::Ring { agent: id.into(), state: RingState::Resting, progress: None });
            self.emit(Event::GoTo { agent: id.into(), station: home });
            let activity = match outcome {
                Outcome::Failed => "the last run failed".to_string(),
                Outcome::Cancelled => "cancelled".to_string(),
                _ => cut(summary.as_deref().unwrap_or("done"), 60),
            };
            self.emit(Event::Activity { agent: id.into(), text: activity });
        }

        let Some(live) = self.agents.get_mut(id).and_then(|a| a.live.as_mut()) else { return };
        // The run history of the Activity app (logs/runs.jsonl).
        let record = meerora_protocol::RunRecord {
            id: run.id.clone(),
            trigger: run.trigger,
            task: run.task.clone(),
            start: run.started,
            end: now,
            outcome,
            summary: summary.clone(),
            tokens: run.tokens,
        };
        if let Err(e) = crate::store::append_run(&path, &record) {
            warn!("cannot write the run history of {id} ({e})");
        }
        live.store.last_run = Some(LastRun {
            id: run.id.clone(),
            trigger: run.trigger,
            task: run.task.clone(),
            started: run.started,
            finished: now,
            outcome,
            summary,
            tokens: run.tokens,
        });
        let commit = live.git.then(|| format!("run {}: {}", local_time(), cut(first_line(&run.task), 72)));
        let queued = std::mem::take(&mut live.queue);
        self.persist(id);
        if let Some(message) = commit {
            commit_in_background(path, message);
        }
        self.expire_grants(id, &run.id);
        // A .git that appeared in a workspace folder during the run was not read-only for it.
        let folders = self.agents.get(id).map(|a| a.dir.config.workspace.clone()).unwrap_or_default();
        let home = meerora_runtime::sandbox::user_home();
        let (workspace, _) = meerora_protocol::workspace::resolve(&folders, home.as_deref(), &self.opts.data_dir);
        if let Err(e) = meerora_protocol::workspace::record_run_end(&self.opts.data_dir, id, &workspace) {
            warn!("cannot record the workspace repositories of {id} ({e})");
        }
        if continuing {
            let note = crate::capabilities::continuation_note(&run.continuation);
            let task = std::iter::once(note).chain(queued).collect::<Vec<_>>().join("\n\n");
            self.start_run(id, run.trigger, task, None, false);
        } else if !queued.is_empty() {
            self.start_run(id, Trigger::User, queued.join("\n\n"), None, false);
        }
    }
}

fn commit_in_background(dir: PathBuf, message: String) {
    let spawned = std::thread::Builder::new().name("git".into()).spawn(move || {
        let _guard = GIT.lock().unwrap_or_else(|e| e.into_inner());
        match git::commit(&dir, &message) {
            Ok(true) => debug!("committed {} ({message})", dir.display()),
            Ok(false) => {}
            Err(e) => warn!("the commit in {} failed ({e})", dir.display()),
        }
    });
    if let Err(e) = spawned {
        warn!("cannot start the git thread ({e})");
    }
}

/// Starts the run on this thread and forwards its events to the daemon until
/// it finishes. Panics of the runtime become errors of the run.
fn forward(
    factory: RuntimeFactory,
    config: RuntimeConfig,
    settings: Settings,
    request: RunRequest,
    run: String,
    cancel: Arc<AtomicBool>,
    tx: Sender<Msg>,
) {
    let agent = request.agent.clone();
    let send = |event: RunEvent| tx.send(Msg::Run { agent: agent.clone(), run: run.clone(), event }).is_ok();
    let started = catch_unwind(AssertUnwindSafe(|| -> anyhow::Result<_> {
        let runtime: Box<dyn Runtime> = factory(&config, &settings)?;
        let handle = runtime.start(request)?;
        Ok((runtime, handle))
    }));
    // The runtime stays alive until the run ends.
    let (_runtime, handle) = match started {
        Ok(Ok(started)) => started,
        Ok(Err(e)) => {
            send(RunEvent::Finished { result: Err(format!("The run could not start ({e:#}).")) });
            return;
        }
        Err(panic) => {
            send(RunEvent::Finished { result: Err(format!("The runtime crashed ({}).", panic_text(&*panic))) });
            return;
        }
    };
    let mut cancelled = false;
    loop {
        if !cancelled && cancel.load(Ordering::SeqCst) {
            handle.cancel();
            cancelled = true;
        }
        match handle.events.recv_timeout(Duration::from_millis(100)) {
            Ok(event) => {
                let last = matches!(event, RunEvent::Finished { .. });
                if !send(event) || last {
                    return;
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                let reason = if cancelled { "cancelled" } else { "The runtime stopped without a result." };
                send(RunEvent::Finished { result: Err(reason.into()) });
                return;
            }
        }
    }
}

pub(crate) fn panic_text(panic: &(dyn Any + Send)) -> String {
    panic
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| panic.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "a panic".into())
}

/// [`system_prompt_with`] for an agent without workspace folders.
#[cfg(test)]
pub fn system_prompt(agent: &AgentDir, stations: &[String]) -> String {
    system_prompt_with(agent, stations, &[])
}

/// meerora's part of the system prompt, followed by the agent's instructions.
/// `workspace` holds the agent's checked workspace folders.
pub fn system_prompt_with(agent: &AgentDir, stations: &[String], workspace: &[PathBuf]) -> String {
    let config = &agent.config;
    // Agents live in `<data dir>/agents/<id>` (D21).
    let data_dir = agent.path.parent().and_then(std::path::Path::parent).filter(|d| !d.as_os_str().is_empty());
    let (api, examples) = match data_dir {
        Some(d) => (script_api_path(d).display().to_string(), meerora_protocol::space_dir(d).join("devices").display().to_string()),
        None => ("API.md".to_string(), "the devices/ folder of the space".to_string()),
    };
    let mut may = vec!["read and edit files in your directory"];
    if config.capabilities.has("shell") {
        may.push("run commands in your directory");
    }
    if config.capabilities.has("web") {
        may.push("fetch web pages and search the web");
    }
    if config.capabilities.has("apps.launch") {
        may.push("start desktop apps on your desktop, where they run in your sandbox");
    }
    let may = match may.split_last() {
        Some((last, [])) => last.to_string(),
        Some((last, rest)) => format!("{} and {last}", rest.join(", ")),
        None => String::new(),
    };
    let mut prompt = format!(
        "You are {name}, an agent that lives as a small figure in the user's meerora space. The space is a small 3D room on the \
user's computer where every agent is visible, and the user watches you work there.

Your directory {dir} is your home. Keep notes and long-term memory in memory/, put results and downloads in artifacts/ and your \
own settings in config/. meerora commits the directory to git after each run, except artifacts/ and logs/. You may {may}.

meerora gives you these tools:
- ask_user shows the user a result or asks for a decision. Your run then ends waiting for them, with your question on their card. \
Use it when you need an answer or want them to look at something.
- figure_set changes your figure through the params your figure script declares.
- set_activity sets the short line under your name, for example \"reading the inbox\". Keep it under 40 characters.
- note posts a short progress note in your chat panel.
- go_to walks you to a station of the space ({stations}). You walk to the station of the tools you use by yourself, so you \
rarely need it.
- request_capability asks the user for a capability you lack: shell for commands in your directory, web for fetching pages \
and searching, apps.launch for starting desktop apps on your desktop. Give a short reason. The call waits until the user \
decides. When they allow it, end your turn as the answer says, and meerora continues this conversation in a new run that has \
it. Ask only when the task needs it.

You can see and use the windows of your view, which the user watches, through five more tools (D30). Every window is an app \
instance with a handle: chat, terminal:1, browser:1, editor:1, files:1, viewer:1, git:1, changes, activity, and window:7 for a real \
app window.
- apps_list lists the open instances with their handles.
- app_read returns what an instance shows as text, with the elements you can act on. Each element has a ref such as e3.
- app_screenshot returns a picture of a browser page or a real app window.
- app_act performs one action. Terminal: type {{text}}, key {{keys}} such as ctrl+c or enter, run {{command}}. Browser: open {{url}}, \
click {{ref}}, type {{ref, text, submit}}, scroll {{dy}}, back, forward, reload. Editor, files and viewer: open {{path}}, and goto \
{{line}} in the editor. Git: stage {{path}}, unstage {{path}}, commit {{message}}. Pushing stays with the user. Real app window: \
click {{x, y}}, type {{text}}, key {{keys}}, scroll {{dx, dy}}. Every instance: focus, close.
- app_open opens a built-in app (terminal, browser with a url, editor, files or viewer with a path) or starts a real app by its \
desktop id or program, and returns the new handle.
They need capabilities: apps.read to look, apps.control to act on and open built-in apps, apps.input to act on real app windows \
and apps.launch to start real apps. Ask for them with request_capability. They work as soon as the user allows them, so you do \
not end your turn for them. Everything you do there shows in the user's action log. When the user takes over an instance, your \
actions on it fail until they hand it back.

You can make devices. A device is an object in the space with a shape, a live state binding and actions. Write its script to \
devices/<name>.js in your directory, with a lowercase name such as paper_plane.js. meerora loads it within a few seconds and \
tells you in this chat whether it loaded. The script API is in {api}, read its device section before you write a device. The \
devices of the space in {examples} are working examples. The script builds its shape like a figure script and ends with export \
default device({{ name, parts, params, place, bind, actions, status }}). place is {{ at: [x, y, z] }} or {{ station: \"window\", \
offset: [x, y, z] }}. bind maps a param to one of these sources: system.disk, system.cpu, system.memory, system.network, \
network.airplane_mode, clock.time, agent.<id>.param.<name> and agent.<id>.ring. An action looks like \
action(\"network.airplane_mode.write\", \"toggle\"). A device may only bind and use capabilities you hold, so ask for \
system.disk.read, network.airplane_mode.read or the capability of an action with request_capability first.

Keep your answers short and plain, because the user reads them in a small panel. Start your final answer with one sentence that \
sums it up. It appears on your card.",
        name = config.name,
        dir = agent.path.display(),
        stations = stations.join(", "),
    );
    if !workspace.is_empty() {
        let folders = workspace.iter().map(|w| w.display().to_string()).collect::<Vec<_>>().join(", ");
        let commands = if config.capabilities.has("shell") { ", and run commands there with absolute paths or cd" } else { "" };
        prompt.push_str(&format!(
            "\n\nThe user also gave you these workspace folders: {folders}. You may read and edit files there{commands}. Your \
runs still start in your own directory, so use their absolute paths. Their .git folders are read-only for you, so you cannot \
stage, commit or change git settings there. The user reviews your changes in the Git app and commits them."
        ));
    }
    // The personal and the news agent get their extra tools explained (D26, D28).
    if let Some(addition) = crate::social::prompt_addition(config.role) {
        prompt.push_str("\n\n");
        prompt.push_str(addition);
    }
    // The helper gets meerora explained and its configuration tools (helper.rs).
    if crate::helper::is_helper(config) {
        prompt.push_str("\n\n");
        prompt.push_str(crate::helper::prompt_addition());
    }
    if let Some(instructions) = config.instructions.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        prompt.push_str("\n\nStanding instructions from the user follow.\n\n");
        prompt.push_str(instructions);
    }
    prompt
}

/// The script API (`meerora-script/API.md`) that agents read to write devices.
const SCRIPT_API: &str = include_str!("../../meerora-script/API.md");

/// Where the daemon keeps a copy of the script API for agents, inside the
/// data dir so their sandbox can show it read-only. The app names the same
/// path when it asks an agent for a device (ui/device_list.rs).
pub fn script_api_path(data_dir: &std::path::Path) -> PathBuf {
    data_dir.join("docs").join("API.md")
}

/// Writes the script API to [`script_api_path`] when it is missing or out of
/// date and returns its path, or `None` when it cannot be written.
pub(crate) fn script_api(data_dir: &std::path::Path) -> Option<PathBuf> {
    let path = script_api_path(data_dir);
    if std::fs::read_to_string(&path).is_ok_and(|text| text == SCRIPT_API) {
        return Some(path);
    }
    let written = path.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|()| std::fs::write(&path, SCRIPT_API));
    match written {
        Ok(()) => Some(path),
        Err(e) => {
            warn!("cannot write the script API to {} ({e}), agents run without it", path.display());
            None
        }
    }
}

/// The activity line for a tool call, or None for meerora's own tools, which
/// set the activity themselves.
pub fn activity_for(tool: &str, summary: &str) -> Option<String> {
    if tool.starts_with("meerora.") {
        return None;
    }
    let summary = first_line(summary);
    let text = match tool {
        "terminal.run" => format!("running {summary}"),
        "editor.patch" => format!("editing {summary}"),
        "files.read" => format!("reading {summary}"),
        "browser.open" => format!("opening {summary}"),
        "browser.search" => format!("searching {summary}"),
        other => format!("using {}", other.strip_prefix("tool.").unwrap_or(other)),
    };
    Some(cut(&text, 60))
}

/// At most `max` characters, with an ellipsis when cut.
pub fn cut(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut short: String = text.chars().take(max.saturating_sub(1)).collect();
    short.truncate(short.trim_end().len());
    short.push('…');
    short
}

/// The first line that is not empty.
pub fn first_line(text: &str) -> &str {
    text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("")
}

/// The first sentence of the first line, for the needs-you card.
pub fn first_sentence(text: &str) -> Option<String> {
    let line = first_line(text);
    if line.is_empty() {
        return None;
    }
    let chars: Vec<char> = line.chars().collect();
    let end = (0..chars.len())
        .find(|&i| matches!(chars[i], '.' | '!' | '?') && chars.get(i + 1).is_none_or(|c| c.is_whitespace()))
        .map_or(chars.len(), |i| i + 1);
    Some(cut(&chars[..end].iter().collect::<String>(), 120))
}

pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// The local date, for the token budget.
pub fn today() -> String {
    jiff::Zoned::now().date().to_string()
}

/// Local date and time for commit messages.
pub fn local_time() -> String {
    jiff::Zoned::now().strftime("%Y-%m-%d %H:%M").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_helpers() {
        assert_eq!(first_sentence("Done. All tests pass."), Some("Done.".into()));
        assert_eq!(first_sentence("\n\nVersion 1.2 is out! Next"), Some("Version 1.2 is out!".into()));
        assert_eq!(first_sentence("no stop at all"), Some("no stop at all".into()));
        assert_eq!(first_sentence("  \n"), None);
        assert_eq!(cut("abcdef", 4), "abc…");
        assert_eq!(cut("abc", 4), "abc");
        assert_eq!(activity_for("terminal.run", "cargo test\n--all"), Some("running cargo test".into()));
        assert_eq!(activity_for("tool.Grep", "x"), Some("using Grep".into()));
        assert_eq!(activity_for("meerora.set_activity", "x"), None);
    }

    #[test]
    fn the_preamble_names_the_agent_its_rights_and_tools() {
        let mut agent = crate::scenario::tests::agent(
            "wren",
            "name = \"Wren\"\nhome = \"window\"\ninstructions = \"Only mail.\"\n[capabilities]\ngranted = [\"web\"]",
        );
        agent.path = PathBuf::from("/data/agents/wren");
        let prompt = system_prompt(&agent, &["door".into(), "mailbox".into()]);
        for needle in [
            "You are Wren",
            "/data/agents/wren",
            "memory/",
            "artifacts/",
            "config/",
            "ask_user",
            "figure_set",
            "set_activity",
            "request_capability",
            "door, mailbox",
            "devices/<name>.js",
            "device({ name",
            "/data/docs/API.md",
            "/data/spaces/",
            "system.cpu",
        ] {
            assert!(prompt.contains(needle), "missing {needle}");
        }
        assert!(prompt.contains("read and edit files in your directory and fetch web pages"));
        assert!(!prompt.contains("run commands"));
        assert!(prompt.ends_with("Only mail."));
        assert!(!prompt.contains("workspace folders"));

        let with = system_prompt_with(&agent, &[], &[PathBuf::from("/home/u/code/project"), PathBuf::from("/srv/site")]);
        assert!(with.contains("workspace folders: /home/u/code/project, /srv/site"), "{with}");
        assert!(with.contains(".git folders are read-only") && with.contains("Git app"));
        assert!(with.ends_with("Only mail."), "the standing instructions stay last");
    }

    #[test]
    fn the_script_api_is_kept_for_the_sandbox() {
        let dir = std::env::temp_dir().join(format!("meerora-script-api-{}-{}", std::process::id(), now_ms()));
        let path = script_api(&dir).expect("written");
        assert_eq!(path, dir.join("docs/API.md"));
        assert!(std::fs::read_to_string(&path).unwrap().contains("## device({...})"));
        std::fs::write(&path, "old").unwrap();
        assert_eq!(script_api(&dir), Some(path.clone()));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), SCRIPT_API, "an old copy is replaced");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
