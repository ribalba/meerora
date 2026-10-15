//! The daemon's state and how it reacts to clients, runs, the builder and the
//! clock. Everything runs on one thread. Runs and builder sessions live on
//! their own threads and report back through [`Msg`].

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use jiff::Zoned;
use meerora_protocol::{
    AgentDir, Command, Event, PROTOCOL_VERSION, RingState, Role, RuntimeConfig, Settings, ToolCall, Trigger, load_agents,
};
use meerora_runtime::{McpBridge, RunEvent, Runtime};
use tracing::{debug, info, warn};

use crate::capabilities::Broker;
use crate::engine::Engine;
use crate::figures::FigureCache;
use crate::git::{self, Repo};
use crate::runs::{cut, now_ms};
use crate::scenario::{Scenario, is_mock};
use crate::schedule::{QuietHours, When, entry_key};
use crate::server::{Clock, DAEMON_NAME};
use crate::store::{REPLAY, Store, StoredMessage, append_message, load_messages};
use crate::{builder, space};

/// Creates the runtime of a run. Tests pass a fake.
pub type RuntimeFactory = Arc<dyn Fn(&RuntimeConfig, &Settings) -> anyhow::Result<Box<dyn Runtime>> + Send + Sync>;

/// The real runtimes of `meerora-runtime`.
pub fn real_runtimes() -> RuntimeFactory {
    Arc::new(|config: &RuntimeConfig, settings: &Settings| meerora_runtime::create(config, settings))
}

/// The longest the daemon sleeps before it looks at the clock again.
const MAX_WAIT: Duration = Duration::from_secs(10);

/// What reaches the daemon thread.
pub enum Msg {
    Connected {
        id: u64,
        stream: UnixStream,
        /// A client of an agent's bridge socket (`server::bridge_listener`).
        scoped: bool,
    },
    Command {
        id: u64,
        command: Command,
    },
    Gone {
        id: u64,
    },
    /// An event of a run, from the thread that forwards it.
    Run {
        agent: String,
        run: String,
        event: RunEvent,
    },
    /// A builder session made progress.
    Draft(Event),
    /// A search or import of the index finished.
    Index(crate::index::Done),
    /// A request to the index for the personal agent or the feed finished (D26, D27).
    Social(crate::social::Done),
    /// A one-shot question of the command box grew or ended (ask.rs).
    Answer {
        client: u64,
        event: Event,
    },
    /// An answer for one client that a thread worked out, such as the
    /// helper's `config_read check` (config_tools.rs).
    Reply {
        client: u64,
        event: Event,
    },
}

/// An event for every client that said hello, or for one client.
#[derive(Clone, Debug, PartialEq)]
pub enum Outgoing {
    All(Event),
    To(u64, Event),
}

/// How the daemon was started.
pub struct Options {
    pub data_dir: PathBuf,
    pub socket: PathBuf,
    pub settings: Settings,
    pub stations: Vec<String>,
    pub factory: RuntimeFactory,
    /// The `meerora-mcp` binary, when it exists.
    pub bridge: Option<PathBuf>,
    /// Keep agent directories in git (D21).
    pub git: bool,
    /// Speed of the mock scenario.
    pub speed: f64,
}

pub struct Daemon {
    pub(crate) opts: Options,
    pub(crate) engine: Engine,
    pub(crate) clock: Clock,
    pub(crate) agents: BTreeMap<String, Agent>,
    pub(crate) figures: FigureCache,
    pub(crate) tx: Sender<Msg>,
    pub(crate) out: Vec<Outgoing>,
    /// Capability requests and grants (D8, Q4).
    pub(crate) caps: Broker,
    /// The socket of each agent's MCP bridge (`server::bridge_listener`).
    pub(crate) bridge_sockets: BTreeMap<String, PathBuf>,
    /// Post drafts, the news digest and the account (D26 to D28).
    pub(crate) social: crate::social::Social,
    /// Devices, their sources and grants (D7, D8).
    pub(crate) devices: crate::devices::DeviceHub,
    /// The trash for retired agents that are deleted (graveyard.rs).
    pub(crate) graveyard: crate::graveyard::Graveyard,
    /// One-shot questions of the command box that are running (ask.rs).
    pub(crate) asks: crate::ask::Asks,
    /// The app clients and the app tool calls that wait for the app (D30, app_tools.rs).
    pub(crate) app_tools: crate::app_tools::AppTools,
    /// The helper's configuration changes that wait for the user (config_tools.rs).
    pub(crate) proposals: crate::config_tools::Proposals,
    seq: u64,
}

/// One agent directory and what the daemon keeps about it beyond the engine state.
pub(crate) struct Agent {
    pub dir: AgentDir,
    /// The last messages, replayed on hello.
    pub messages: VecDeque<StoredMessage>,
    /// Present for agents with a real runtime.
    pub live: Option<Live>,
}

/// A real agent (D22).
pub(crate) struct Live {
    pub store: Store,
    pub run: Option<ActiveRun>,
    /// Messages that arrived during a run. They start the next run together.
    pub queue: Vec<String>,
    pub slots: Vec<Slot>,
    pub git: bool,
    /// Local day on which the budget note was posted.
    pub budget_noted: Option<String>,
}

pub(crate) struct ActiveRun {
    pub id: String,
    pub trigger: Trigger,
    pub task: String,
    pub started: u64,
    /// The agent's reply, which grows while the run reports text.
    pub reply_id: String,
    pub reply_at: Option<u64>,
    pub text: String,
    /// Set by `ask_user`. The run then ends waiting for the user.
    pub ask: Option<String>,
    pub cancel: Arc<AtomicBool>,
    pub cancelled: bool,
    /// Queued messages dropped by the cancel.
    pub dropped: usize,
    pub tokens: u64,
    /// The capabilities the run started with (D8).
    pub granted: Vec<String>,
    /// Capabilities granted while it ran. When it ends, a new run continues
    /// the conversation with them (capabilities.rs).
    pub continuation: Vec<String>,
    /// When the run is stopped if it has not ended its turn after a grant.
    pub continue_by: Option<Instant>,
}

/// One schedule entry of a real agent (D23).
pub(crate) struct Slot {
    pub key: String,
    pub when: When,
    pub quiet: Option<QuietHours>,
    pub task: String,
    pub station: Option<String>,
    /// Next wake, unix milliseconds.
    pub next: i64,
    /// When the countdown to `next` began: the last wake, or when the slot
    /// was set up.
    pub since: i64,
}

impl Daemon {
    pub fn new(opts: Options, scenario: Scenario, dirs: Vec<AgentDir>, tx: Sender<Msg>) -> Self {
        let engine = Engine::new(scenario, &dirs);
        let clock = Clock::new(opts.speed);
        let caps = Broker::new(&opts.data_dir);
        let social = crate::social::Social::load(&opts.data_dir);
        let devices = crate::devices::DeviceHub::new(&opts.data_dir);
        let mut daemon = Daemon {
            opts,
            engine,
            clock,
            agents: BTreeMap::new(),
            figures: FigureCache::default(),
            tx,
            out: Vec::new(),
            caps,
            bridge_sockets: BTreeMap::new(),
            social,
            devices,
            graveyard: Default::default(),
            asks: Default::default(),
            app_tools: Default::default(),
            proposals: Default::default(),
            seq: 0,
        };
        let mut git_off: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for dir in dirs {
            let id = dir.id.clone();
            if let Some(reason) = daemon.load_agent(dir) {
                git_off.entry(reason).or_default().push(id);
            }
        }
        for (reason, ids) in git_off {
            info!("git is off for {} because {reason}", ids.join(", "));
        }
        daemon.load_devices();
        daemon
    }

    /// Sets up an agent that is already in the engine. Returns why git is
    /// off for its directory, if it is.
    fn load_agent(&mut self, dir: AgentDir) -> Option<String> {
        let repo = if self.opts.git { git::ensure_repo(&dir.path) } else { Repo::Off("git is switched off".into()) };
        let git_off = match &repo {
            Repo::On => None,
            Repo::Off(reason) => Some(reason.clone()),
        };
        let mut agent = Agent { dir, messages: VecDeque::new(), live: None };
        if !is_mock(&agent.dir.config) {
            let store = Store::load(&agent.dir.path);
            self.restore(&agent.dir.id, &store);
            agent.messages = load_messages(&agent.dir.path, REPLAY).into();
            agent.live = Some(Live {
                slots: slots(&agent.dir, &store, &Zoned::now()),
                store,
                run: None,
                queue: Vec::new(),
                git: repo == Repo::On,
                budget_noted: None,
            });
        }
        self.agents.insert(agent.dir.id.clone(), agent);
        git_off
    }

    /// Puts the params, activity and a waiting ring of `state.json` back into the engine.
    fn restore(&mut self, id: &str, store: &Store) {
        for (name, value) in &store.params {
            self.engine.apply(&Event::Param { agent: id.into(), name: name.clone(), value: value.clone() });
        }
        if !store.activity.is_empty() {
            self.engine.apply(&Event::Activity { agent: id.into(), text: store.activity.clone() });
        }
        if store.ring == Some(RingState::NeedsYou) {
            self.engine.apply(&Event::Ring { agent: id.into(), state: RingState::NeedsYou, progress: None });
        }
    }

    pub fn take_out(&mut self) -> Vec<Outgoing> {
        std::mem::take(&mut self.out)
    }

    /// Updates the engine state and sends the event to every client.
    pub(crate) fn emit(&mut self, event: Event) {
        self.engine.apply(&event);
        self.out.push(Outgoing::All(event));
    }

    pub(crate) fn next_id(&mut self, prefix: &str) -> String {
        self.seq += 1;
        format!("{prefix}-{}-{}", now_ms(), self.seq)
    }

    /// Sends a finished message and keeps it for the replay. Real agents also
    /// write it to `logs/messages.jsonl`.
    pub(crate) fn post(&mut self, agent: &str, id: String, role: Role, text: String, at: u64) {
        self.out.push(Outgoing::All(Event::Message { agent: agent.into(), id: id.clone(), role, text: text.clone(), done: true, at }));
        let Some(a) = self.agents.get_mut(agent) else { return };
        let message = StoredMessage { id, role, text, at };
        if a.live.is_some()
            && let Err(e) = append_message(&a.dir.path, &message)
        {
            warn!("cannot write the conversation of {agent} ({e})");
        }
        a.messages.push_back(message);
        while a.messages.len() > REPLAY {
            a.messages.pop_front();
        }
    }

    /// Posts a note from meerora itself.
    pub(crate) fn post_system(&mut self, agent: &str, text: String) {
        let id = self.next_id(agent);
        self.post(agent, id, Role::System, text, now_ms());
    }

    /// Writes ring, activity and params of a real agent to its `state.json`.
    pub(crate) fn persist(&mut self, id: &str) {
        let Some(state) = self.engine.agent(id) else { return };
        let (ring, activity, params) = (state.ring, state.activity.clone(), state.params.clone());
        let Some(agent) = self.agents.get_mut(id) else { return };
        let Some(live) = agent.live.as_mut() else { return };
        live.store.ring = Some(ring);
        live.store.activity = activity;
        live.store.params = params;
        if let Err(e) = live.store.save(&agent.dir.path) {
            warn!("cannot write the state of {id} ({e})");
        }
    }

    /// Hello, the full state, the recent conversations and the runs in progress.
    pub fn greeting(&self) -> Vec<Event> {
        let mut events = vec![Event::Hello { version: PROTOCOL_VERSION, daemon: DAEMON_NAME.into() }];
        events.extend(self.engine.snapshot());
        for (id, agent) in &self.agents {
            events.extend(replay(id, agent));
            if let Some(live) = &agent.live {
                events.push(next_wake(id, live));
            }
        }
        events.extend(self.capability_greeting());
        events.extend(self.social_greeting());
        events.extend(self.device_greeting());
        events.push(self.graveyard_event());
        events.extend(self.config_greeting());
        events
    }

    /// Plays what is due in the scenario and wakes scheduled agents.
    pub fn tick(&mut self) {
        let events = self.engine.advance(self.clock.now());
        // An agent on its way to the graveyard follows nothing else (graveyard.rs).
        let events: Vec<Outgoing> = events.into_iter().filter(|e| !e.agent().is_some_and(|a| self.walking(a))).map(Outgoing::All).collect();
        self.out.extend(events);
        self.tick_schedules(&Zoned::now());
        self.tick_continuations(Instant::now());
        self.tick_devices(Instant::now());
        self.tick_walks(Instant::now());
        self.tick_apps(Instant::now());
        self.tick_proposals(Instant::now());
    }

    /// How long the daemon may wait for the next message.
    pub fn next_wait(&self) -> Duration {
        let mut wait = MAX_WAIT;
        if let Some(due) = self.engine.next_due() {
            wait = wait.min(self.clock.until(due));
        }
        let now = now_ms() as i64;
        for live in self.agents.values().filter_map(|a| a.live.as_ref()) {
            for slot in &live.slots {
                wait = wait.min(Duration::from_millis(u64::try_from(slot.next - now).unwrap_or(0)));
            }
        }
        if let Some(due) = self.continuation_wait(Instant::now()) {
            wait = wait.min(due);
        }
        if let Some(due) = self.walk_wait(Instant::now()) {
            wait = wait.min(due);
        }
        if let Some(due) = self.apps_wait(Instant::now()) {
            wait = wait.min(due);
        }
        if let Some(due) = self.proposals_wait(Instant::now()) {
            wait = wait.min(due);
        }
        wait.min(self.device_wait(Instant::now()))
    }

    pub fn handle_msg(&mut self, msg: Msg) {
        match msg {
            Msg::Run { agent, run, event } => self.on_run_event(&agent, &run, event),
            Msg::Draft(event) => self.out.push(Outgoing::All(event)),
            Msg::Index(done) => self.on_index(done),
            Msg::Social(done) => self.on_social(done),
            Msg::Answer { client, event } => self.on_answer(client, event),
            Msg::Reply { client, event } => self.out.push(Outgoing::To(client, event)),
            Msg::Command { id, command } => self.handle_command(id, command),
            Msg::Connected { .. } | Msg::Gone { .. } => {}
        }
    }

    /// Everything except `Hello`, which the server answers with [`Daemon::greeting`].
    pub fn handle_command(&mut self, client: u64, command: Command) {
        match command {
            Command::Hello { .. } => {}
            Command::Inject { event } => {
                debug!("client {client} injects {}", meerora_protocol::encode(&event).trim_end());
                if event.agent().is_some_and(|a| self.walking(a)) {
                    debug!("dropped, the agent is on its way to the graveyard");
                    return;
                }
                if let Some(event) = self.engine.inject(event) {
                    self.out.push(Outgoing::All(event));
                }
            }
            Command::Ack { agent } => self.ack(&agent),
            Command::Say { agent, text } => self.say(&agent, text),
            Command::Cancel { agent } => self.cancel(&agent),
            Command::ReloadAgents => self.reload(),
            Command::Build { session, prompt, script } => self.build(session, prompt, script),
            Command::Tool { id, agent, call: ToolCall::RequestCapability { capability, reason } } => {
                self.request_capability(client, id, &agent, capability, reason)
            }
            Command::Tool {
                id,
                agent,
                call: call @ (ToolCall::DraftPost { .. } | ToolCall::ReadFeed { .. } | ToolCall::PostDigest { .. }),
            } => self.social_tool(client, id, &agent, call),
            Command::Tool {
                id,
                agent,
                call:
                    call @ (ToolCall::AppsList
                    | ToolCall::AppRead { .. }
                    | ToolCall::AppScreenshot { .. }
                    | ToolCall::AppAct { .. }
                    | ToolCall::AppOpen { .. }),
            } => self.app_tool(client, id, &agent, call),
            // The helper's tools, the change waits for the user (config_tools.rs).
            Command::Tool { id, agent, call: call @ (ToolCall::ConfigRead { .. } | ToolCall::ConfigChange { .. }) } => {
                self.config_tool(client, id, &agent, call)
            }
            Command::Tool { id, agent, call } => {
                let (ok, message) = self.tool(&agent, call);
                if !ok {
                    info!("tool call of {agent} refused ({message})");
                }
                self.out.push(Outgoing::To(client, Event::ToolResult { id, ok, message, image: None }));
            }
            Command::DecideCapability { id, approve, remember } => self.decide_capability(&id, approve, remember),
            Command::RevokeCapability { agent, capability } => self.revoke_capability(&agent, &capability),
            Command::IndexSearch { request, query } => self.index_search(client, request, query),
            Command::ImportAgent { manifest } => self.import_agent(client, manifest),
            command @ (Command::CreateAccount { .. }
            | Command::Interview { .. }
            | Command::SaveProfile { .. }
            | Command::Social { .. }
            | Command::ReadFeed { .. }
            | Command::DecidePost { .. }
            | Command::React { .. }
            | Command::AddTemplate { .. }) => self.social_command(client, command),
            Command::DeviceAction { device, action } => self.device_action(client, &device, &action),
            Command::RevokeDeviceCapability { device, capability } => self.revoke_device_capability(&device, &capability),
            Command::RunNow { agent, schedule } => self.run_now(&agent, schedule, &Zoned::now()),
            Command::Retire { agent } => self.retire(client, &agent),
            Command::Restore { agent } => self.restore_retired(client, &agent),
            Command::DeleteRetired { agent } => self.delete_retired(client, &agent),
            Command::Arrived { agent, station } => self.arrived(&agent, &station),
            Command::DeviceSetting { device, name, value } => self.device_setting(client, &device, &name, value),
            Command::Ask { request, agent, question } => self.ask(client, request, &agent, question),
            Command::CancelAsk { request } => self.cancel_ask(&request),
            Command::ServeApps => self.serve_apps(client),
            Command::AppReply { id, result } => self.app_reply(id, result),
            Command::DecideConfig { id, apply } => self.decide_config(client, &id, apply),
        }
    }

    fn ack(&mut self, id: &str) {
        // A running agent that waits for a capability decision keeps its run (D8).
        if self.agents.get(id).and_then(|a| a.live.as_ref()).is_some_and(|l| l.run.is_some()) {
            info!("ack for {id} ignored, its run goes on");
            return;
        }
        let events = self.engine.ack(id, self.clock.now());
        self.out.extend(events.into_iter().map(Outgoing::All));
        if self.agents.get(id).is_some_and(|a| a.live.is_some()) {
            self.persist(id);
        }
    }

    fn say(&mut self, id: &str, text: String) {
        let text = text.trim().to_string();
        let Some(agent) = self.agents.get(id) else {
            warn!("message for unknown agent {id}");
            return;
        };
        if text.is_empty() {
            return;
        }
        let name = agent.dir.config.name.clone();
        match agent.live.as_ref().map(|live| live.run.is_some()) {
            None => {
                let you = self.next_id(id);
                self.post(id, you, Role::User, text, now_ms());
                self.post_system(
                    id,
                    format!("{name} is a mock agent that plays the demo scenario. Give it a runtime in its agent.toml to talk to it."),
                );
            }
            Some(true) => {
                let you = self.next_id(id);
                self.post(id, you, Role::User, text.clone(), now_ms());
                if let Some(live) = self.agents.get_mut(id).and_then(|a| a.live.as_mut()) {
                    live.queue.push(text);
                    info!("{id} is busy, so the message waits for the next run ({} queued)", live.queue.len());
                }
            }
            Some(false) => self.start_run(id, Trigger::User, text, None, true),
        }
    }

    pub(crate) fn cancel(&mut self, id: &str) {
        self.deny_requests(id, "the run was cancelled");
        self.close_proposals(id, "the run was cancelled");
        let Some(live) = self.agents.get_mut(id).and_then(|a| a.live.as_mut()) else {
            info!("cancel for {id} ignored, it is not a real agent");
            return;
        };
        let dropped = std::mem::take(&mut live.queue).len();
        let Some(run) = live.run.as_mut() else {
            info!("cancel for {id} ignored, it has no run");
            return;
        };
        info!("cancelling run {} of {id}", run.id);
        run.cancelled = true;
        run.continuation.clear();
        run.dropped = dropped;
        run.cancel.store(true, Ordering::SeqCst);
    }

    /// Answers a meerora tool call from the MCP bridge (D22, D24).
    fn tool(&mut self, id: &str, call: ToolCall) -> (bool, String) {
        let Some(agent) = self.agents.get(id) else {
            return (false, format!("unknown agent {id:?}"));
        };
        let figure = agent.dir.figure_path();
        let running = agent.live.as_ref().is_some_and(|l| l.run.is_some());
        match call {
            ToolCall::FigureSet { name, value } => match self.figures.check(&figure, &name, &value) {
                Ok(()) => {
                    let message = format!("{name} is now {value}");
                    self.emit(Event::Param { agent: id.into(), name, value });
                    self.persist(id);
                    (true, message)
                }
                Err(e) => (false, e),
            },
            ToolCall::GoTo { station } => {
                if self.opts.stations.contains(&station) {
                    self.emit(Event::GoTo { agent: id.into(), station: station.clone() });
                    (true, format!("walking to {station}"))
                } else {
                    (false, format!("{station:?} is not a station. The stations are {}.", self.opts.stations.join(", ")))
                }
            }
            ToolCall::SetActivity { text } => {
                let text = cut(text.trim(), 80);
                self.emit(Event::Activity { agent: id.into(), text: text.clone() });
                self.persist(id);
                (true, format!("activity is now {text:?}"))
            }
            ToolCall::AskUser { question } => {
                let question = question.trim().to_string();
                if question.is_empty() {
                    return (false, "the question is empty".into());
                }
                let message = self.next_id(id);
                self.post(id, message, Role::Agent, question.clone(), now_ms());
                if running {
                    if let Some(run) = self.agents.get_mut(id).and_then(|a| a.live.as_mut()).and_then(|l| l.run.as_mut()) {
                        run.ask = Some(question);
                    }
                    (true, "The user sees your question. Finish your turn, and the run ends waiting for them.".into())
                } else {
                    self.emit(Event::Ring { agent: id.into(), state: RingState::NeedsYou, progress: None });
                    self.emit(Event::Activity { agent: id.into(), text: cut(&question, 60) });
                    self.persist(id);
                    (true, "The user sees your question.".into())
                }
            }
            // Waits for the user, so handle_command gives it to the broker.
            ToolCall::RequestCapability { capability, .. } => (false, format!("{capability} goes through the capability broker")),
            // Talk to the index, so handle_command gives them to social.rs.
            ToolCall::DraftPost { .. } | ToolCall::ReadFeed { .. } | ToolCall::PostDigest { .. } => {
                (false, "this tool goes through the social module".into())
            }
            // Wait for the app, so handle_command gives them to app_tools.rs.
            ToolCall::AppsList
            | ToolCall::AppRead { .. }
            | ToolCall::AppScreenshot { .. }
            | ToolCall::AppAct { .. }
            | ToolCall::AppOpen { .. } => (false, "this tool goes through the app".into()),
            // Wait for the user, so handle_command gives them to config_tools.rs.
            ToolCall::ConfigRead { .. } | ToolCall::ConfigChange { .. } => (false, "this tool goes through config_tools".into()),
            ToolCall::Note { text } => {
                let text = text.trim().to_string();
                if text.is_empty() {
                    return (false, "the note is empty".into());
                }
                let message = self.next_id(id);
                self.post(id, message, Role::Agent, text, now_ms());
                (true, "noted".into())
            }
        }
    }

    /// Rescans the agent directories, the space and `meerora.toml`.
    fn reload(&mut self) {
        let data_dir = self.opts.data_dir.clone();
        match Settings::load(&data_dir) {
            Ok(settings) => self.opts.settings = settings,
            Err(e) => warn!("keeping the previous settings because {e}"),
        }
        let (stations, warning) = space::stations_or_fallback(&data_dir);
        if let Some(warning) = warning {
            warn!("{warning}");
        }
        self.opts.stations = stations;
        self.devices.rescan_soon();

        for result in load_agents(&data_dir) {
            match result {
                Ok(dir) if self.agents.contains_key(&dir.id) => self.update_agent(dir),
                Ok(dir) => self.add_agent(dir),
                Err(e) => warn!("{e}"),
            }
        }
        // A broken agent.toml keeps its agent. Only a vanished directory removes it.
        let present = agent_dir_names(&data_dir);
        let gone: Vec<String> = self.agents.keys().filter(|id| !present.contains(*id)).cloned().collect();
        for id in gone {
            self.remove_agent(&id);
        }
    }

    pub(crate) fn add_agent(&mut self, dir: AgentDir) {
        let id = dir.id.clone();
        info!("agent {id} appeared");
        self.engine.add_agent(&dir, self.clock.now());
        if let Some(reason) = self.load_agent(dir) {
            info!("git is off for {id} because {reason}");
        }
        let mut events = self.engine.snapshot_agent(&id);
        if let Some(agent) = self.agents.get(&id) {
            events.extend(replay(&id, agent));
            if let Some(live) = &agent.live {
                events.push(next_wake(&id, live));
            }
        }
        self.out.extend(events.into_iter().map(Outgoing::All));
    }

    fn update_agent(&mut self, dir: AgentDir) {
        let id = dir.id.clone();
        let Some(agent) = self.agents.get(&id) else { return };
        if agent.dir.config == dir.config {
            return;
        }
        info!("agent {id} changed");
        if self.engine.update_agent(&dir) {
            let state = self.engine.agent(&id).map(|s| (s.name.clone(), s.system));
            if let Some((name, system)) = state {
                self.out.push(Outgoing::All(Event::AgentSpawned { agent: id.clone(), name, system }));
            }
        }
        let was_real = agent.live.is_some();
        let now_real = !is_mock(&dir.config);
        if was_real && !now_real {
            self.cancel(&id);
        }
        if !was_real && now_real {
            // Becoming real is like appearing again, minus the engine state.
            self.load_agent(dir);
            if let Some(live) = self.agents.get(&id).and_then(|a| a.live.as_ref()) {
                let wake = next_wake(&id, live);
                self.out.push(Outgoing::All(wake));
            }
            return;
        }
        let Some(agent) = self.agents.get_mut(&id) else { return };
        let mut wake = None;
        if now_real {
            if let Some(live) = agent.live.as_mut() {
                live.slots = slots(&dir, &live.store, &Zoned::now());
                wake = Some(next_wake(&id, live));
            }
        } else {
            agent.live = None;
            wake = Some(Event::NextWake { agent: id.clone(), at: None, since: 0 });
        }
        agent.dir = dir;
        if let Some(wake) = wake {
            self.out.push(Outgoing::All(wake));
        }
    }

    pub(crate) fn remove_agent(&mut self, id: &str) {
        info!("agent {id} is gone");
        self.deny_requests(id, "the agent was removed");
        self.close_proposals(id, "the agent was removed");
        self.cancel(id);
        self.agents.remove(id);
        self.engine.remove_agent(id);
        self.out.push(Outgoing::All(Event::AgentRemoved { agent: id.into() }));
    }

    fn build(&mut self, session: String, prompt: String, script: Option<String>) {
        info!("builder session {session} starts");
        let job = builder::BuildJob { session, prompt, script };
        let config = self.opts.settings.builder.clone().unwrap_or_else(builder::default_runtime);
        let settings = self.opts.settings.clone();
        let factory = self.opts.factory.clone();
        let tx = self.tx.clone();
        let spawned = std::thread::Builder::new().name("builder".into()).spawn(move || {
            builder::run(&job, &config, &settings, &factory, &mut |event| {
                let _ = tx.send(Msg::Draft(event));
            });
        });
        if let Err(e) = spawned {
            warn!("cannot start a builder thread ({e})");
        }
    }

    /// The MCP bridge command for an agent's run, when the binary exists.
    /// The bridge talks to the agent's own socket, which only takes tool
    /// calls as that agent (D8, D16).
    pub(crate) fn bridge(&mut self, id: &str) -> Option<McpBridge> {
        let command = self.opts.bridge.clone()?;
        let socket = self.bridge_socket(id)?;
        let args = vec![
            "--agent".to_string(),
            id.to_string(),
            "--data-dir".into(),
            self.opts.data_dir.display().to_string(),
            // The sandbox shows the room's folder but not meerora.toml, so the bridge gets the folder.
            "--space".into(),
            meerora_protocol::space_dir(&self.opts.data_dir).display().to_string(),
            "--socket".into(),
            socket.display().to_string(),
        ];
        Some(McpBridge { command, args })
    }

    /// The agent's bridge socket, opened on first use. Without it the run
    /// goes without the meerora tools, never through the main socket.
    pub(crate) fn bridge_socket(&mut self, id: &str) -> Option<PathBuf> {
        if let Some(path) = self.bridge_sockets.get(id) {
            return Some(path.clone());
        }
        match crate::server::bridge_listener(&self.opts.socket, id, self.tx.clone()) {
            Ok(path) => {
                self.bridge_sockets.insert(id.to_string(), path.clone());
                Some(path)
            }
            Err(e) => {
                warn!("{id} runs without the meerora tools, because its bridge socket failed ({e:#})");
                None
            }
        }
    }

    /// Wakes real agents whose schedule is due (D23).
    pub(crate) fn tick_schedules(&mut self, now: &Zoned) {
        let now_ms = now.timestamp().as_millisecond();
        let today = now.date().to_string();
        let ids: Vec<String> = self.agents.keys().cloned().collect();
        for id in ids {
            let Some(agent) = self.agents.get_mut(&id) else { continue };
            let limit = agent.dir.config.budget.as_ref().and_then(|b| b.tokens_per_day);
            let Some(live) = agent.live.as_mut() else { continue };
            let busy = live.run.is_some() || !live.queue.is_empty();
            let (mut start, mut dirty, mut budget_note) = (None, false, false);
            for slot in &mut live.slots {
                if slot.next > now_ms {
                    continue;
                }
                slot.next = slot.when.next_after(now);
                slot.since = now_ms;
                live.store.schedule.insert(slot.key.clone(), now_ms);
                dirty = true;
                if slot.quiet.is_some_and(|q| q.contains(now.time())) {
                    info!("{id} sleeps through its wake ({}) in quiet hours", slot.key);
                } else if busy || start.is_some() {
                    info!("{id} is busy and skips its wake ({})", slot.key);
                } else if let Some(limit) = limit.filter(|limit| live.store.tokens_on(&today) >= *limit) {
                    info!("{id} used its {limit} tokens for today and skips its wake ({})", slot.key);
                    if live.budget_noted.as_deref() != Some(today.as_str()) {
                        live.budget_noted = Some(today.clone());
                        budget_note = true;
                    }
                } else {
                    start = Some((slot.task.clone(), slot.station.clone()));
                }
            }
            if dirty {
                let wake = next_wake(&id, live);
                self.out.push(Outgoing::All(wake));
            }
            if budget_note {
                let limit = limit.unwrap_or_default();
                self.post_system(&id, format!("Scheduled runs wait until tomorrow, because today's budget of {limit} tokens is used up."));
            }
            if dirty {
                self.persist(&id);
            }
            if let Some((task, station)) = start {
                info!("{id} wakes on its schedule");
                self.start_run(&id, Trigger::Schedule, task, station, false);
            }
        }
    }

    /// Starts the task of the wake-up at `index` of the agent's schedule now
    /// ([`Command::RunNow`] from the Activity app). Like a due wake in
    /// [`Daemon::tick_schedules`] it records the wake, restarts the
    /// countdown and respects the token budget. Quiet hours do not hold it
    /// back, because you asked for it.
    pub(crate) fn run_now(&mut self, id: &str, index: usize, now: &Zoned) {
        let now_ms = now.timestamp().as_millisecond();
        let today = now.date().to_string();
        let Some(agent) = self.agents.get_mut(id) else {
            warn!("run now for unknown agent {id}");
            return;
        };
        let Some(entry) = agent.dir.config.schedule.get(index).cloned() else {
            warn!("run now for {id} ignored, it has no wake-up {index}");
            return;
        };
        let limit = agent.dir.config.budget.as_ref().and_then(|b| b.tokens_per_day);
        let name = agent.dir.config.name.clone();
        let Some(live) = agent.live.as_mut() else {
            info!("run now for {id} ignored, it is not a real agent");
            return;
        };
        if live.run.is_some() || !live.queue.is_empty() {
            info!("run now for {id} ignored, it is busy");
            self.post_system(id, format!("{name} is busy, so the wake-up did not start. Try again when the run has ended."));
            return;
        }
        if let Some(limit) = limit.filter(|limit| live.store.tokens_on(&today) >= *limit) {
            info!("run now for {id} refused, it used its {limit} tokens for today");
            self.post_system(id, format!("The wake-up did not start, because today's budget of {limit} tokens is used up."));
            return;
        }
        let key = entry_key(&entry);
        if let Some(slot) = live.slots.iter_mut().find(|s| s.key == key && s.task == entry.task) {
            slot.next = slot.when.next_after(now);
            slot.since = now_ms;
        }
        live.store.schedule.insert(key, now_ms);
        let wake = next_wake(id, live);
        self.out.push(Outgoing::All(wake));
        self.persist(id);
        info!("{id} runs its wake-up {index} now");
        self.start_run(id, Trigger::Schedule, entry.task, entry.station, false);
    }
}

/// The stored messages of an agent and its run in progress, as events.
fn replay(id: &str, agent: &Agent) -> Vec<Event> {
    let mut events: Vec<Event> = agent
        .messages
        .iter()
        .map(|m| Event::Message { agent: id.into(), id: m.id.clone(), role: m.role, text: m.text.clone(), done: true, at: m.at })
        .collect();
    if let Some(run) = agent.live.as_ref().and_then(|l| l.run.as_ref()) {
        events.push(Event::RunStarted {
            agent: id.into(),
            run: run.id.clone(),
            trigger: run.trigger,
            task: run.task.clone(),
            started: run.started,
        });
        if !run.text.is_empty() {
            let at = run.reply_at.unwrap_or(run.started);
            events.push(Event::Message {
                agent: id.into(),
                id: run.reply_id.clone(),
                role: Role::Agent,
                text: run.text.clone(),
                done: false,
                at,
            });
        }
    }
    events
}

/// The schedule entries of a real agent with their next wake.
fn slots(dir: &AgentDir, store: &Store, now: &Zoned) -> Vec<Slot> {
    let mut slots = Vec::new();
    for entry in &dir.config.schedule {
        let when = match When::parse(entry) {
            Ok(when) => when,
            Err(e) => {
                warn!("a schedule entry of {} is ignored because {e}", dir.id);
                continue;
            }
        };
        let quiet = match entry.quiet_hours.as_deref().map(QuietHours::parse) {
            Some(Ok(quiet)) => Some(quiet),
            Some(Err(e)) => {
                warn!("the quiet hours of {} are ignored because {e}", dir.id);
                None
            }
            None => None,
        };
        let key = entry_key(entry);
        let last = store.schedule.get(&key).copied();
        let next = when.first(last, now);
        let since = last.filter(|t| *t < next).unwrap_or_else(|| now.timestamp().as_millisecond());
        slots.push(Slot { key, when, quiet, task: entry.task.clone(), station: entry.station.clone(), next, since });
    }
    slots
}

/// The countdown to the next wake of a real agent (D23).
fn next_wake(id: &str, live: &Live) -> Event {
    let slot = live.slots.iter().min_by_key(|s| s.next);
    let ms = |t: i64| u64::try_from(t).unwrap_or(0);
    Event::NextWake { agent: id.into(), at: slot.map(|s| ms(s.next)), since: slot.map_or(0, |s| ms(s.since)) }
}

/// Names of the directories below `agents/` that hold an `agent.toml`.
fn agent_dir_names(data_dir: &Path) -> BTreeSet<String> {
    let Ok(entries) = std::fs::read_dir(data_dir.join("agents")) else { return BTreeSet::new() };
    entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().join("agent.toml").is_file())
        .filter_map(|e| e.file_name().to_str().map(String::from))
        .collect()
}
