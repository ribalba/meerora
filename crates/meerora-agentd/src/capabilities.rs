//! The capability broker (D8, closes Q4).
//!
//! An agent asks for a capability it lacks with the `request_capability` tool
//! of its MCP bridge. The app shows a card with three answers: Allow once,
//! Always allow and Deny. The tool call waits until the user decides.
//!
//! The Q4 decisions:
//! - Granularity is one capability name per agent. `shell` and `web` change
//!   what a run may do (D29). Dotted names such as
//!   `network.airplane_mode.write` are for devices (D7, D8, devices.rs).
//! - Always allow writes the name into `[capabilities] granted` of
//!   `agent.toml`. Allow once holds until the agent's next run ends, which is
//!   the run that continues the conversation with it.
//! - `RevokeCapability` takes a capability away, from `agent.toml` and from
//!   the grants that hold once. A run that uses `shell` or `web` is stopped
//!   when it loses it, because a CLI cannot drop a permission while it runs.
//! - Every request, decision, revocation and expiry is one line of
//!   `<data dir>/audit.jsonl` with the time, the agent, the capability, the
//!   decision and who decided.
//! - System agents (D10) get nothing without a request either.
//!
//! A granted capability takes effect without the user typing anything
//! again. The CLIs fix their permissions when they start, so the answer
//! tells the run that asked to end its turn. When it has ended, the daemon
//! continues the same conversation (the same runtime session) in a new run
//! with the new permissions and a short note. A run that does not end within
//! [`CONTINUE_GRACE`] is stopped and continued the same way.
//!
//! The app learns that a request was closed from
//! [`Event::CapabilitiesChanged`]. It drops the open requests of that agent,
//! and the daemon sends the ones that are still open right after it.
//!
//! The bridges reach the daemon through a socket of their own per agent
//! (`server::bridge_listener`), which only takes tool calls as that agent.
//! So an agent inside its sandbox cannot decide its own requests.
//!
//! Devices of the space (D7) ask through the same broker when the user
//! clicks an action whose capability is not approved yet (`devices.rs`).
//! Their requests are decided with the same `DecideCapability` and logged in
//! the same audit log, with `device` in place of `agent`.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use meerora_protocol::{AgentConfig, Event, RingState, Trigger};
use serde::Serialize;
use tracing::{info, warn};

use crate::daemon::{Daemon, Outgoing};
use crate::runs::cut;

/// How long the run that asked may take to end its turn after a grant.
pub const CONTINUE_GRACE: Duration = Duration::from_secs(30);
/// Longest capability name.
const NAME_MAX: usize = 64;
/// Longest reason, in characters.
const REASON_MAX: usize = 300;
/// The capabilities that change what a runtime may do (D29).
const RUNTIME_CAPABILITIES: [&str; 2] = ["shell", "web"];

/// Capabilities the daemon checks on every call, the app tools of D30
/// (app_tools.rs). A grant takes effect at once, so it needs no new run, and
/// Allow once holds for the run that is going on.
pub(crate) fn per_call(capability: &str) -> bool {
    capability.starts_with("apps.")
}

/// Open requests, grants that hold once, and the audit log.
pub(crate) struct Broker {
    audit: PathBuf,
    open: Vec<Request>,
    once: Vec<Once>,
    /// Open requests of devices of the space (D7, D8).
    pub(crate) devices: Vec<DeviceRequest>,
}

/// A request of a device of the space that the user has not decided yet.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct DeviceRequest {
    pub id: String,
    pub device: String,
    pub capability: String,
    /// The label of the action, for the card.
    pub action: String,
    /// The clients that clicked and the action ids that wait for the decision.
    pub waiters: Vec<(u64, String)>,
}

/// One line of `audit.jsonl` about a device of the space.
#[derive(Serialize)]
pub(crate) struct DeviceEntry<'a> {
    pub time: String,
    pub device: &'a str,
    pub capability: &'a str,
    /// request, decision, revoke or use
    pub event: &'a str,
    /// allow_once, always_allow or deny for decisions, toggle, on or off for a use.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decision: Option<&'a str>,
    /// user, meerora, or the device for its own request.
    pub by: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<&'a str>,
}

impl<'a> DeviceEntry<'a> {
    pub fn new(device: &'a str, capability: &'a str, event: &'a str, by: &'a str) -> Self {
        Self { time: String::new(), device, capability, event, decision: None, by, request: None, note: None }
    }
}

/// A request the user has not decided yet.
#[derive(Clone, Debug, PartialEq)]
struct Request {
    id: String,
    agent: String,
    capability: String,
    reason: String,
    /// The bridges waiting for the decision, as client and tool call id.
    waiters: Vec<(u64, String)>,
}

/// An Allow once grant.
#[derive(Clone, Debug, PartialEq)]
struct Once {
    agent: String,
    capability: String,
    /// The run that uses it. The grant ends with that run.
    run: Option<String>,
}

/// One line of `audit.jsonl`.
#[derive(Serialize)]
struct Entry<'a> {
    time: String,
    agent: &'a str,
    capability: &'a str,
    /// request, decision, revoke, expire or use (app tools, D30)
    event: &'a str,
    /// allow_once, always_allow or deny, for decisions.
    #[serde(skip_serializing_if = "Option::is_none")]
    decision: Option<&'a str>,
    /// user, meerora, or the agent for its own request.
    by: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    request: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<&'a str>,
}

impl Broker {
    pub fn new(data_dir: &Path) -> Self {
        Self { audit: data_dir.join("audit.jsonl"), open: Vec::new(), once: Vec::new(), devices: Vec::new() }
    }

    /// Writes one line about a device to the audit log. A failure is logged, it never stops a decision.
    pub(crate) fn log_device(&self, entry: DeviceEntry) {
        let entry = DeviceEntry { time: jiff::Zoned::now().strftime("%Y-%m-%dT%H:%M:%S%:z").to_string(), ..entry };
        let written = serde_json::to_string(&entry).map_err(std::io::Error::other).and_then(|line| {
            std::fs::OpenOptions::new().create(true).append(true).open(&self.audit)?.write_all(format!("{line}\n").as_bytes())
        });
        if let Err(e) = written {
            warn!("cannot write the audit log {} ({e})", self.audit.display());
        }
    }

    /// Writes one line of the audit log. A failure is logged, it never stops a decision.
    fn log(&self, entry: Entry) {
        let entry = Entry { time: jiff::Zoned::now().strftime("%Y-%m-%dT%H:%M:%S%:z").to_string(), ..entry };
        let line = serde_json::to_string(&entry).map(|mut l| {
            l.push('\n');
            l
        });
        let written = line
            .map_err(std::io::Error::other)
            .and_then(|line| std::fs::OpenOptions::new().create(true).append(true).open(&self.audit)?.write_all(line.as_bytes()));
        if let Err(e) = written {
            warn!("cannot write the audit log {} ({e})", self.audit.display());
        }
    }

    fn entry<'a>(agent: &'a str, capability: &'a str, event: &'a str, by: &'a str) -> Entry<'a> {
        Entry { time: String::new(), agent, capability, event, decision: None, by, request: None, note: None }
    }

    /// A `use` line for an app tool call of `agent` (D30). `refused` is set
    /// when the agent did not hold the capability, `note` names the call.
    pub(crate) fn log_use(&self, agent: &str, capability: &str, refused: bool, note: &str) {
        let decision = refused.then_some("refused");
        self.log(Entry { decision, note: Some(note), ..Broker::entry(agent, capability, "use", agent) });
    }

    /// The Allow once grants of `agent`, which the run `run` now uses.
    fn claim(&mut self, agent: &str, run: &str) -> Vec<String> {
        let mut claimed = Vec::new();
        for grant in self.once.iter_mut().filter(|g| g.agent == agent) {
            grant.run.get_or_insert_with(|| run.to_string());
            claimed.push(grant.capability.clone());
        }
        claimed
    }

    fn once_of(&self, agent: &str) -> impl Iterator<Item = &str> {
        self.once.iter().filter(move |g| g.agent == agent).map(|g| g.capability.as_str())
    }
}

/// Checks that a capability name is made of lowercase words of letters,
/// digits and `_`, joined by dots.
pub(crate) fn check_name(name: &str) -> Result<(), String> {
    let word = |w: &str| {
        w.starts_with(|c: char| c.is_ascii_lowercase()) && w.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    };
    if name.len() <= NAME_MAX && name.split('.').all(word) {
        Ok(())
    } else {
        Err(format!(
            "{name:?} is not a capability name. Use lowercase words joined by dots, for example web, shell or network.airplane_mode.write."
        ))
    }
}

/// "web", "web and shell", "web, shell and mail.read".
fn list(items: &[String]) -> String {
    match items.split_last() {
        Some((last, [])) => last.clone(),
        Some((last, rest)) => format!("{} and {last}", rest.join(", ")),
        None => String::new(),
    }
}

/// The task of the run that continues a conversation after a grant.
pub(crate) fn continuation_note(capabilities: &[String]) -> String {
    format!("The user allowed you {}. Continue the task.", list(capabilities))
}

impl Daemon {
    /// What the agent holds now, from `agent.toml` and the Allow once grants.
    pub(crate) fn granted(&self, agent: &str) -> Vec<String> {
        let mut granted = self.agents.get(agent).map(|a| a.dir.config.capabilities.granted.clone()).unwrap_or_default();
        for capability in self.caps.once_of(agent) {
            if !granted.iter().any(|g| g == capability) {
                granted.push(capability.to_string());
            }
        }
        granted
    }

    /// The capabilities of a run that starts now. Allow once grants are
    /// claimed by it and end with it.
    pub(crate) fn claim_capabilities(&mut self, agent: &str, run: &str) -> Vec<String> {
        let mut granted = self.agents.get(agent).map(|a| a.dir.config.capabilities.granted.clone()).unwrap_or_default();
        for capability in self.caps.claim(agent, run) {
            if !granted.contains(&capability) {
                granted.push(capability);
            }
        }
        granted
    }

    /// Ends the Allow once grants that the run `run` used.
    pub(crate) fn expire_grants(&mut self, agent: &str, run: &str) {
        let (ended, kept): (Vec<Once>, Vec<Once>) =
            std::mem::take(&mut self.caps.once).into_iter().partition(|g| g.agent == agent && g.run.as_deref() == Some(run));
        self.caps.once = kept;
        if ended.is_empty() {
            return;
        }
        for grant in &ended {
            self.caps.log(Entry {
                note: Some("the run it was allowed for ended"),
                ..Broker::entry(agent, &grant.capability, "expire", "meerora")
            });
        }
        self.capabilities_changed(agent);
    }

    /// Ends every Allow once grant of an agent that leaves the space
    /// (graveyard.rs), whether a run claimed it or not.
    pub(crate) fn end_once_grants(&mut self, agent: &str, why: &str) {
        let (ended, kept): (Vec<Once>, Vec<Once>) = std::mem::take(&mut self.caps.once).into_iter().partition(|g| g.agent == agent);
        self.caps.once = kept;
        for grant in &ended {
            self.caps.log(Entry { note: Some(why), ..Broker::entry(agent, &grant.capability, "expire", "meerora") });
        }
    }

    /// `ToolCall::RequestCapability` from the bridge `client`. The answer
    /// to `call` waits for the user's decision.
    pub(crate) fn request_capability(&mut self, client: u64, call: String, agent: &str, capability: String, reason: String) {
        let refuse = |daemon: &mut Daemon, message: String| {
            info!("capability request of {agent} refused ({message})");
            daemon.out.push(Outgoing::To(client, Event::ToolResult { id: call.clone(), ok: false, message, image: None }));
        };
        let (capability, reason) = (capability.trim().to_string(), cut(reason.trim(), REASON_MAX));
        let Some(a) = self.agents.get(agent) else { return refuse(self, format!("unknown agent {agent:?}")) };
        let Some(live) = a.live.as_ref() else { return refuse(self, format!("{agent} has no runtime, so it cannot ask for anything")) };
        if let Err(e) = check_name(&capability) {
            return refuse(self, e);
        }
        if reason.is_empty() {
            return refuse(self, "Say in the reason why you need it.".into());
        }
        let in_run = live.run.as_ref().is_some_and(|r| r.granted.contains(&capability));
        if self.granted(agent).contains(&capability) {
            let message = if in_run {
                format!("You already hold {capability}.")
            } else {
                // Held, but this run started without it.
                self.continue_with(agent, &capability)
            };
            self.out.push(Outgoing::To(client, Event::ToolResult { id: call, ok: true, message, image: None }));
            return;
        }
        if let Some(open) = self.caps.open.iter_mut().find(|r| r.agent == agent && r.capability == capability) {
            open.waiters.push((client, call));
            return;
        }
        let id = self.next_id("cap");
        info!("{agent} asks for {capability} ({id})");
        self.caps.log(Entry { request: Some(&id), note: Some(&reason), ..Broker::entry(agent, &capability, "request", agent) });
        let event =
            Event::CapabilityRequest { id: id.clone(), agent: agent.into(), capability: capability.clone(), reason: reason.clone() };
        self.caps.open.push(Request { id, agent: agent.into(), capability: capability.clone(), reason, waiters: vec![(client, call)] });
        self.out.push(Outgoing::All(event));
        // The figure waits for the user with a full ring (D3).
        self.emit(Event::Ring { agent: agent.into(), state: RingState::NeedsYou, progress: None });
        self.emit(Event::Activity { agent: agent.into(), text: cut(&format!("asks for {capability}"), 60) });
    }

    /// `Command::DecideCapability` from the user.
    pub(crate) fn decide_capability(&mut self, id: &str, approve: bool, remember: bool) {
        if self.decide_device_capability(id, approve, remember) {
            return;
        }
        let Some(index) = self.caps.open.iter().position(|r| r.id == id) else {
            info!("capability request {id} is unknown or already decided");
            return;
        };
        let request = self.caps.open.remove(index);
        let (agent, capability) = (request.agent.as_str(), request.capability.as_str());
        let decision = match (approve, remember) {
            (false, _) => "deny",
            (true, false) => "allow_once",
            (true, true) => "always_allow",
        };
        info!("the user decided {decision} for {capability} of {agent} ({id})");
        let mut note = None;
        if approve && remember {
            if let Err(e) = self.set_granted(agent, capability, true) {
                warn!("cannot save {capability} for {agent} ({e}), so it holds once");
                note = Some(format!("agent.toml could not be written ({e}), so it holds once"));
                self.caps.once.push(Once { agent: agent.into(), capability: capability.into(), run: None });
            }
        } else if approve {
            // A capability checked per call serves the run that asked, which ends it.
            let run = per_call(capability)
                .then(|| self.agents.get(agent).and_then(|a| a.live.as_ref()).and_then(|l| l.run.as_ref()).map(|r| r.id.clone()))
                .flatten();
            self.caps.once.push(Once { agent: agent.into(), capability: capability.into(), run });
        }
        self.caps.log(Entry {
            decision: Some(decision),
            request: Some(id),
            note: note.as_deref(),
            ..Broker::entry(agent, capability, "decision", "user")
        });
        let message = if approve {
            self.continue_with(agent, capability)
        } else {
            format!("The user did not allow {capability}. Go on without it, and tell the user what you could not do.")
        };
        for (client, call) in &request.waiters {
            self.out.push(Outgoing::To(*client, Event::ToolResult { id: call.clone(), ok: true, message: message.clone(), image: None }));
        }
        self.capabilities_changed(agent);
        let activity = if approve { format!("continues with {capability}") } else { format!("{capability} was not allowed") };
        self.waiting_done(agent, activity);
    }

    /// Makes a granted capability take effect. The running run is continued
    /// in a new one once it ends, and an idle agent continues right away.
    /// Returns the answer for the run that asked.
    fn continue_with(&mut self, agent: &str, capability: &str) -> String {
        if per_call(capability) {
            return format!("The user allowed {capability}. You can use it right away. There is no need to end your turn.");
        }
        let Some(live) = self.agents.get_mut(agent).and_then(|a| a.live.as_mut()) else { return format!("The user allowed {capability}.") };
        match live.run.as_mut() {
            Some(run) => {
                if !run.continuation.iter().any(|c| c == capability) {
                    run.continuation.push(capability.to_string());
                }
                run.continue_by.get_or_insert_with(|| Instant::now() + CONTINUE_GRACE);
                format!(
                    "The user allowed {capability}. End your turn now with one short sentence. meerora continues this conversation \
                     right away in a new run that has {capability}."
                )
            }
            None => {
                self.start_run(agent, Trigger::User, continuation_note(&[capability.to_string()]), None, false);
                format!("The user allowed {capability}.")
            }
        }
    }

    /// The ring of a running agent goes back to working once no request of
    /// it is open any more.
    fn waiting_done(&mut self, agent: &str, activity: String) {
        let running = self.agents.get(agent).and_then(|a| a.live.as_ref()).is_some_and(|l| l.run.is_some());
        if running && !self.caps.open.iter().any(|r| r.agent == agent) {
            self.emit(Event::Ring { agent: agent.into(), state: RingState::Working, progress: None });
            self.emit(Event::Activity { agent: agent.into(), text: cut(&activity, 60) });
        }
    }

    /// `Command::RevokeCapability` from the user.
    pub(crate) fn revoke_capability(&mut self, agent: &str, capability: &str) {
        let Some(a) = self.agents.get(agent) else {
            warn!("cannot revoke {capability} from unknown agent {agent}");
            return;
        };
        let in_config = a.dir.config.capabilities.has(capability);
        let in_run = a.live.as_ref().and_then(|l| l.run.as_ref()).is_some_and(|r| r.granted.iter().any(|g| g == capability));
        let before = self.caps.once.len();
        self.caps.once.retain(|g| !(g.agent == agent && g.capability == capability));
        let held_once = self.caps.once.len() != before;
        if !in_config && !held_once {
            info!("{agent} does not hold {capability}, nothing to revoke");
            self.capabilities_changed(agent);
            return;
        }
        let mut note = None;
        if in_config && let Err(e) = self.set_granted(agent, capability, false) {
            warn!("cannot remove {capability} from the agent.toml of {agent} ({e})");
            note = Some(format!("agent.toml could not be written ({e})"));
            self.post_system(agent, format!("{capability} could not be removed from agent.toml ({e}). Remove it there by hand."));
        }
        info!("the user revoked {capability} from {agent}");
        self.caps.log(Entry { note: note.as_deref(), ..Broker::entry(agent, capability, "revoke", "user") });
        self.capabilities_changed(agent);
        if in_run && RUNTIME_CAPABILITIES.contains(&capability) {
            self.post_system(agent, format!("The run stopped, because {capability} was revoked while it used it."));
            self.cancel(agent);
        }
    }

    /// Denies the open requests of `agent`, for example because its run was
    /// cancelled or the agent was removed.
    pub(crate) fn deny_requests(&mut self, agent: &str, why: &str) {
        let (closed, open): (Vec<Request>, Vec<Request>) = std::mem::take(&mut self.caps.open).into_iter().partition(|r| r.agent == agent);
        self.caps.open = open;
        if closed.is_empty() {
            return;
        }
        for request in &closed {
            info!("capability request {} of {agent} is denied, because {why}", request.id);
            self.caps.log(Entry {
                decision: Some("deny"),
                request: Some(&request.id),
                note: Some(why),
                ..Broker::entry(agent, &request.capability, "decision", "meerora")
            });
            let message = format!("The request for {} was closed, because {why}.", request.capability);
            for (client, call) in &request.waiters {
                let result = Event::ToolResult { id: call.clone(), ok: true, message: message.clone(), image: None };
                self.out.push(Outgoing::To(*client, result));
            }
        }
        self.capabilities_changed(agent);
    }

    /// The capabilities of every real agent and the open requests, for the greeting.
    pub(crate) fn capability_greeting(&self) -> Vec<Event> {
        let mut events: Vec<Event> = self
            .agents
            .iter()
            .filter(|(_, a)| a.live.is_some())
            .map(|(id, _)| Event::CapabilitiesChanged { agent: id.clone(), granted: self.granted(id) })
            .collect();
        events.extend(self.caps.open.iter().map(request_event));
        events
    }

    /// Tells the clients what the agent holds now and which of its requests
    /// are still open (see the module documentation).
    fn capabilities_changed(&mut self, agent: &str) {
        let granted = self.granted(agent);
        self.out.push(Outgoing::All(Event::CapabilitiesChanged { agent: agent.into(), granted }));
        let still_open: Vec<Event> = self.caps.open.iter().filter(|r| r.agent == agent).map(request_event).collect();
        self.out.extend(still_open.into_iter().map(Outgoing::All));
    }

    /// Stops runs that were told to end their turn after a grant and did not.
    pub(crate) fn tick_continuations(&mut self, now: Instant) {
        for (id, agent) in &mut self.agents {
            let Some(run) = agent.live.as_mut().and_then(|l| l.run.as_mut()) else { continue };
            if run.continue_by.is_some_and(|due| due <= now) {
                info!("{id} did not end its turn after a grant, so the run is stopped and continued");
                run.continue_by = None;
                run.cancel.store(true, Ordering::SeqCst);
            }
        }
    }

    /// How long until the next run has to be stopped for a continuation.
    pub(crate) fn continuation_wait(&self, now: Instant) -> Option<Duration> {
        self.agents.values().filter_map(|a| a.live.as_ref()?.run.as_ref()?.continue_by).min().map(|due| due.saturating_duration_since(now))
    }

    /// Adds or removes a capability in `agent.toml` and in memory.
    fn set_granted(&mut self, agent: &str, capability: &str, on: bool) -> Result<(), String> {
        let a = self.agents.get_mut(agent).ok_or_else(|| format!("unknown agent {agent}"))?;
        let mut granted = a.dir.config.capabilities.granted.clone();
        granted.retain(|g| g != capability);
        if on {
            granted.push(capability.to_string());
        }
        let config = write_granted(&a.dir.path.join("agent.toml"), &granted)?;
        a.dir.config = config;
        Ok(())
    }
}

fn request_event(r: &Request) -> Event {
    Event::CapabilityRequest { id: r.id.clone(), agent: r.agent.clone(), capability: r.capability.clone(), reason: r.reason.clone() }
}

/// Rewrites `[capabilities] granted` of an `agent.toml` and returns the new
/// config. Only that line changes when the file allows it. Otherwise the file
/// is written anew with its leading comment lines kept, as the builder does.
pub(crate) fn write_granted(path: &Path, granted: &[String]) -> Result<AgentConfig, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let old: AgentConfig = toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut wanted = old;
    wanted.capabilities.granted = granted.to_vec();
    let value = toml::Value::Array(granted.iter().map(|g| toml::Value::String(g.clone())).collect());
    let edited =
        edit_granted(&text, &format!("granted = {value}")).filter(|new| toml::from_str::<AgentConfig>(new).ok().as_ref() == Some(&wanted));
    let new = match edited {
        Some(new) => new,
        None => {
            let header: String = text.lines().take_while(|l| l.starts_with('#')).map(|l| format!("{l}\n")).collect();
            format!("{header}{}", toml::to_string(&wanted).map_err(|e| e.to_string())?)
        }
    };
    // Never through a symlink the agent put next to its agent.toml (safefs).
    meerora_protocol::safefs::replace_file(path, new.as_bytes()).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(wanted)
}

/// `text` with the `granted` line of `[capabilities]` replaced or added.
/// None when the table has a form that this cannot edit line by line.
fn edit_granted(text: &str, line: &str) -> Option<String> {
    let mut lines: Vec<String> = text.lines().map(String::from).collect();
    match lines.iter().position(|l| l.trim() == "[capabilities]") {
        None => {
            if lines.iter().any(|l| l.trim_start().starts_with("capabilities")) {
                return None;
            }
            while lines.last().is_some_and(|l| l.trim().is_empty()) {
                lines.pop();
            }
            lines.extend([String::new(), "[capabilities]".into(), line.into()]);
        }
        Some(header) => {
            let end = lines[header + 1..].iter().position(|l| l.trim_start().starts_with('[')).map_or(lines.len(), |p| header + 1 + p);
            let key = (header + 1..end).find(|&i| lines[i].split('=').next().is_some_and(|k| k.trim() == "granted"));
            match key {
                Some(i) => {
                    let value = lines[i].split_once('=')?.1;
                    if value.matches('[').count() != value.matches(']').count() {
                        return None;
                    }
                    lines[i] = line.into();
                }
                None => lines.insert(header + 1, line.into()),
            }
        }
    }
    let mut out = lines.join("\n");
    out.push('\n');
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::tests::TempDir;

    #[test]
    fn names_are_checked() {
        for good in ["web", "shell", "network.airplane_mode.write", "mail.read2"] {
            assert!(check_name(good).is_ok(), "{good}");
        }
        for bad in ["", "Web", "web.", ".web", "web shell", "../x", "a.b-c", "1web", &"x".repeat(65)] {
            assert!(check_name(bad).is_err(), "{bad}");
        }
        assert_eq!(list(&["web".into()]), "web");
        assert_eq!(list(&["web".into(), "shell".into(), "x".into()]), "web, shell and x");
    }

    #[test]
    fn agent_toml_keeps_its_comments() {
        let dir = TempDir::new("granted");
        let path = dir.0.join("agent.toml");
        let text = "# Wren reads the mail.\n# Second comment.\nname = \"Wren\"\nhome = \"window\" # by the window\n\n[runtime]\nkind = \"claude-cli\"\n\n[capabilities]\n# what Wren may do\ngranted = [\"mail.read\"]\n\n[budget]\ntokens_per_day = 5\n";
        std::fs::write(&path, text).unwrap();
        let config = write_granted(&path, &["mail.read".into(), "web".into()]).unwrap();
        assert_eq!(config.capabilities.granted, ["mail.read", "web"]);
        let new = std::fs::read_to_string(&path).unwrap();
        assert_eq!(new, text.replace("granted = [\"mail.read\"]", "granted = [\"mail.read\", \"web\"]"), "only one line changes");

        // A file without the table gets one.
        std::fs::write(&path, "# Quill\nname = \"Quill\"\nhome = \"desk\"\n\n").unwrap();
        write_granted(&path, &["shell".into()]).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "# Quill\nname = \"Quill\"\nhome = \"desk\"\n\n[capabilities]\ngranted = [\"shell\"]\n"
        );

        // A table without the key gets the line.
        std::fs::write(&path, "name = \"R\"\nhome = \"desk\"\n[capabilities]\nsystem = true\n").unwrap();
        let config = write_granted(&path, &["web".into()]).unwrap();
        assert!(config.capabilities.system);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "name = \"R\"\nhome = \"desk\"\n[capabilities]\ngranted = [\"web\"]\nsystem = true\n"
        );

        // A multi-line array is rewritten, the leading comments stay.
        std::fs::write(&path, "# keep me\nname = \"R\"\nhome = \"desk\"\n[capabilities]\ngranted = [\n  \"web\",\n]\n").unwrap();
        let config = write_granted(&path, &[]).unwrap();
        assert!(config.capabilities.granted.is_empty());
        let new = std::fs::read_to_string(&path).unwrap();
        assert!(new.starts_with("# keep me\n"), "{new}");
        assert_eq!(toml::from_str::<AgentConfig>(&new).unwrap(), config);
        assert!(!dir.0.join("agent.toml.tmp").exists());
    }
}
