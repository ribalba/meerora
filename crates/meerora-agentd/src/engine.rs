//! What the clients see of every agent (ring, station, activity, params) and
//! the player of the scenario for mock agents. Real agents change this state
//! through [`Engine::apply`] and are skipped by the scenario.
//!
//! The engine has no clock of its own. The caller passes the current scenario
//! time in seconds, which keeps the engine deterministic and easy to test.
//! Everything it changes comes back as protocol events for the clients.

use std::cmp::{Ordering, Reverse};
use std::collections::{BTreeMap, BinaryHeap};

use meerora_protocol::{AgentDir, Event, RingState};
use serde_json::Value;
use tracing::{debug, info, warn};

use crate::scenario::{HOME, Run, Scenario, Step, is_mock, wake_intervals};

/// Seconds between two chunks of typed terminal input.
pub const TYPE_INTERVAL: f64 = 0.06;
/// Characters per chunk of typed terminal input.
const TYPE_CHUNK: usize = 2;
/// Home of agents that appear through an injected `AgentSpawned`. External
/// agents arrive and leave through the door (D25).
const ARRIVAL_STATION: &str = "door";

/// What the daemon knows about one agent.
#[derive(Clone, Debug, PartialEq)]
pub struct AgentState {
    pub name: String,
    pub system: bool,
    pub home: String,
    pub station: String,
    pub ring: RingState,
    pub progress: Option<f32>,
    pub activity: String,
    pub params: BTreeMap<String, Value>,
    /// Played by the scenario. Real agents are driven by their runtime.
    pub mock: bool,
    run: Option<ActiveRun>,
    runs_started: u64,
}

impl AgentState {
    fn new(name: String, system: bool, home: String, mock: bool) -> Self {
        Self {
            name,
            system,
            mock,
            station: home.clone(),
            home,
            ring: RingState::Resting,
            progress: None,
            activity: String::new(),
            params: BTreeMap::new(),
            run: None,
            runs_started: 0,
        }
    }
}

/// A scheduled run that has not finished. It ends when its last step played
/// and the agent does not need you, or when you acknowledge it.
#[derive(Clone, Debug, PartialEq)]
struct ActiveRun {
    id: u64,
    pending: usize,
    auto_ack: bool,
}

#[derive(Debug)]
enum Job {
    LoopStart,
    Step { agent: String, step: Step, run: Option<u64> },
    Wake { agent: String, every: f64 },
    Type { agent: String, data: String },
    AutoAck { agent: String, run: u64 },
}

#[derive(Debug)]
struct Pending {
    due: f64,
    seq: u64,
    job: Job,
}

impl PartialEq for Pending {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Pending {}

impl PartialOrd for Pending {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Pending {
    fn cmp(&self, other: &Self) -> Ordering {
        self.due.total_cmp(&other.due).then(self.seq.cmp(&other.seq))
    }
}

/// Plays the timeline, wakes scheduled agents and keeps the state that a
/// client receives when it says hello.
pub struct Engine {
    agents: BTreeMap<String, AgentState>,
    timeline: Vec<Step>,
    loop_seconds: Option<f64>,
    runs: BTreeMap<String, Run>,
    queue: BinaryHeap<Reverse<Pending>>,
    seq: u64,
    next_run: u64,
}

impl Engine {
    /// Agents start at their home station, resting. The timeline starts at
    /// time 0 and every `every` schedule entry wakes its agent once per interval.
    pub fn new(scenario: Scenario, agents: &[AgentDir]) -> Self {
        let mut timeline = scenario.steps;
        timeline.sort_by(|a, b| a.at.total_cmp(&b.at));
        let mut engine = Self {
            agents: agents.iter().map(|a| (a.id.clone(), state_of(a))).collect(),
            timeline,
            loop_seconds: scenario.loop_seconds,
            runs: scenario.runs,
            queue: BinaryHeap::new(),
            seq: 0,
            next_run: 0,
        };
        engine.push(0.0, Job::LoopStart);
        for agent in agents {
            engine.schedule_wakes(agent, 0.0);
        }
        engine
    }

    fn schedule_wakes(&mut self, agent: &AgentDir, now: f64) {
        if !is_mock(&agent.config) {
            return;
        }
        let first_after = self.runs.get(&agent.id).and_then(|r| r.first_after);
        for every in wake_intervals(agent) {
            self.push(now + first_after.unwrap_or(every), Job::Wake { agent: agent.id.clone(), every });
        }
    }

    /// Adds an agent that appeared on disk. It starts at home, resting.
    pub fn add_agent(&mut self, agent: &AgentDir, now: f64) {
        self.agents.insert(agent.id.clone(), state_of(agent));
        self.schedule_wakes(agent, now);
    }

    /// Takes over a changed agent.toml. Returns true when the name or the
    /// system flag changed, which clients learn from a new `AgentSpawned`.
    pub fn update_agent(&mut self, agent: &AgentDir) -> bool {
        let Some(state) = self.agents.get_mut(&agent.id) else { return false };
        let changed = state.name != agent.config.name || state.system != agent.config.capabilities.system;
        state.name = agent.config.name.clone();
        state.system = agent.config.capabilities.system;
        state.home = agent.config.home.clone();
        state.mock = is_mock(&agent.config);
        if !state.mock {
            state.run = None;
        }
        changed
    }

    pub fn remove_agent(&mut self, id: &str) {
        self.agents.remove(id);
        // Its wakes end with it, so an agent that comes back (graveyard.rs)
        // does not wake twice as often.
        self.queue.retain(|Reverse(p)| !matches!(&p.job, Job::Wake { agent, .. } if agent == id));
    }

    /// The current state of an agent.
    pub fn agent(&self, id: &str) -> Option<&AgentState> {
        self.agents.get(id)
    }

    /// Scenario time of the next pending job.
    pub fn next_due(&self) -> Option<f64> {
        self.queue.peek().map(|Reverse(p)| p.due)
    }

    /// Plays everything that is due at `now` and returns the resulting events.
    pub fn advance(&mut self, now: f64) -> Vec<Event> {
        let mut out = Vec::new();
        while self.queue.peek().is_some_and(|Reverse(p)| p.due <= now) {
            let Some(Reverse(pending)) = self.queue.pop() else { break };
            self.run_job(pending.due, now, pending.job, &mut out);
        }
        out
    }

    /// The full current state as ordinary events: `AgentSpawned` for every
    /// agent, then per agent its ring, station, activity and params.
    pub fn snapshot(&self) -> Vec<Event> {
        let mut out: Vec<Event> =
            self.agents.iter().map(|(id, a)| Event::AgentSpawned { agent: id.clone(), name: a.name.clone(), system: a.system }).collect();
        for id in self.agents.keys() {
            out.extend(self.state_events(id));
        }
        out
    }

    /// `AgentSpawned` and the state of one agent, for an agent that appeared later.
    pub fn snapshot_agent(&self, id: &str) -> Vec<Event> {
        let Some(a) = self.agents.get(id) else { return Vec::new() };
        let mut out = vec![Event::AgentSpawned { agent: id.into(), name: a.name.clone(), system: a.system }];
        out.extend(self.state_events(id));
        out
    }

    fn state_events(&self, id: &str) -> Vec<Event> {
        let Some(a) = self.agents.get(id) else { return Vec::new() };
        let mut out = vec![
            Event::Ring { agent: id.into(), state: a.ring, progress: a.progress },
            Event::GoTo { agent: id.into(), station: a.station.clone() },
            Event::Activity { agent: id.into(), text: a.activity.clone() },
        ];
        for (name, value) in &a.params {
            out.push(Event::Param { agent: id.into(), name: name.clone(), value: value.clone() });
        }
        out
    }

    /// Takes an event from a client as if the daemon had produced it. Returns
    /// the event to broadcast, or `None` if it must not be broadcast.
    pub fn inject(&mut self, event: Event) -> Option<Event> {
        if matches!(event, Event::Hello { .. }) {
            warn!("ignoring an injected hello");
            return None;
        }
        if let Some(agent) = event.agent()
            && !self.agents.contains_key(agent)
            && !matches!(event, Event::AgentSpawned { .. })
        {
            debug!("injected event for unknown agent {agent}");
        }
        self.apply(&event);
        Some(event)
    }

    /// The user attended to `agent`. Plays the ack steps of its run, or rests
    /// it at home when there are none. Ignored unless the agent needs you.
    pub fn ack(&mut self, agent: &str, now: f64) -> Vec<Event> {
        let mut out = Vec::new();
        match self.agents.get(agent) {
            None => warn!("ack for unknown agent {agent}"),
            Some(a) if a.ring != RingState::NeedsYou => info!("ack for {agent} ignored, it does not need you"),
            Some(_) => {
                info!("{agent} acknowledged");
                self.acknowledge(agent, now, &mut out);
            }
        }
        out.extend(self.advance(now));
        out
    }

    fn push(&mut self, due: f64, job: Job) {
        self.seq += 1;
        self.queue.push(Reverse(Pending { due, seq: self.seq, job }));
    }

    fn run_job(&mut self, due: f64, now: f64, job: Job, out: &mut Vec<Event>) {
        match job {
            Job::LoopStart => self.start_loop(due, now),
            Job::Step { agent, step, run } => self.play(&agent, &step, due, run, out),
            Job::Wake { agent, every } => self.wake(&agent, every, due, now),
            Job::Type { agent, data } => {
                if self.agents.contains_key(&agent) {
                    self.emit(Event::TerminalInput { agent, data }, out);
                }
            }
            Job::AutoAck { agent, run } => {
                let waiting =
                    self.agents.get(&agent).is_some_and(|a| a.ring == RingState::NeedsYou && a.run.as_ref().is_some_and(|r| r.id == run));
                if waiting {
                    info!("{agent} acknowledges by itself");
                    self.acknowledge(&agent, due, out);
                }
            }
        }
    }

    fn start_loop(&mut self, due: f64, now: f64) {
        let mut start = due;
        if let Some(length) = self.loop_seconds {
            // After a stall, skip the loops that were missed instead of replaying them.
            while start + length <= now {
                start += length;
            }
            self.push(start + length, Job::LoopStart);
        }
        debug!("timeline starts at {start} s");
        for i in 0..self.timeline.len() {
            let step = self.timeline[i].clone();
            let agent = step.agent.clone().unwrap_or_default();
            self.push(start + step.at, Job::Step { agent, step, run: None });
        }
    }

    fn wake(&mut self, agent: &str, every: f64, due: f64, now: f64) {
        if !self.agents.contains_key(agent) {
            return;
        }
        // Wakes missed during a stall run once, not repeatedly (D23).
        let mut next = due + every;
        while next <= now {
            next += every;
        }
        self.push(next, Job::Wake { agent: agent.into(), every });

        let Some(state) = self.agents.get_mut(agent) else { return };
        if !state.mock {
            return;
        }
        if state.run.is_some() {
            info!("{agent} is still busy with its last run and skips this wake");
            return;
        }
        let Some(run) = self.runs.get(agent) else {
            info!("{agent} wakes, but the scenario has no run for it");
            return;
        };
        state.runs_started += 1;
        let number = state.runs_started;
        let important = run.is_important(number);
        let extra = if important { &run.important } else { &run.quiet };
        let steps: Vec<Step> = run.steps.iter().chain(extra).cloned().collect();
        info!("{agent} wakes for run {number} ({})", if important { "important" } else { "quiet" });
        if steps.is_empty() {
            return;
        }
        self.next_run += 1;
        let id = self.next_run;
        state.run = Some(ActiveRun { id, pending: steps.len(), auto_ack: false });
        for step in steps {
            self.push(due + step.at, Job::Step { agent: agent.into(), step, run: Some(id) });
        }
    }

    fn play(&mut self, agent: &str, step: &Step, now: f64, run: Option<u64>, out: &mut Vec<Event>) {
        let Some(state) = self.agents.get(agent) else {
            debug!("skipping a step of unknown agent {agent}");
            return;
        };
        if !state.mock {
            return;
        }
        if let Some(id) = run
            && state.run.as_ref().map(|r| r.id) != Some(id)
        {
            // The run was acknowledged before this step was due.
            return;
        }
        let (home, ring) = (state.home.clone(), state.ring);

        match (step.ring, step.progress) {
            (Some(state), progress) => self.emit(Event::Ring { agent: agent.into(), state, progress }, out),
            (None, Some(p)) => self.emit(Event::Ring { agent: agent.into(), state: ring, progress: Some(p) }, out),
            (None, None) => {}
        }
        if let Some(station) = &step.go_to {
            let station = if station == HOME { home } else { station.clone() };
            self.emit(Event::GoTo { agent: agent.into(), station }, out);
        }
        if let Some(text) = &step.activity {
            self.emit(Event::Activity { agent: agent.into(), text: text.clone() }, out);
        }
        for (name, value) in &step.param {
            self.emit(Event::Param { agent: agent.into(), name: name.clone(), value: value.clone() }, out);
        }
        if let Some(action) = &step.action {
            self.emit(Event::Action { agent: agent.into(), tool: action.tool.clone(), summary: action.summary.clone(), target: None }, out);
        }
        if let Some(text) = &step.typing {
            let chars: Vec<char> = text.chars().collect();
            for (i, chunk) in chars.chunks(TYPE_CHUNK).enumerate() {
                self.push(now + i as f64 * TYPE_INTERVAL, Job::Type { agent: agent.into(), data: chunk.iter().collect() });
            }
        }

        if run.is_some() {
            self.finish_run_step(agent, now);
        }
    }

    fn finish_run_step(&mut self, agent: &str, now: f64) {
        let auto_ack_after = self.runs.get(agent).and_then(|r| r.auto_ack_after);
        let Some(state) = self.agents.get_mut(agent) else { return };
        let waiting = state.ring == RingState::NeedsYou;
        let Some(active) = state.run.as_mut() else { return };
        active.pending = active.pending.saturating_sub(1);
        let mut auto_ack = None;
        if waiting
            && !active.auto_ack
            && let Some(after) = auto_ack_after
        {
            active.auto_ack = true;
            auto_ack = Some((now + after, active.id));
        }
        if active.pending == 0 && !waiting {
            state.run = None;
        }
        if let Some((due, run)) = auto_ack {
            self.push(due, Job::AutoAck { agent: agent.into(), run });
        }
    }

    fn acknowledge(&mut self, agent: &str, now: f64, out: &mut Vec<Event>) {
        let Some(state) = self.agents.get_mut(agent) else { return };
        state.run = None;
        let home = state.home.clone();
        let steps = if state.mock { self.runs.get(agent).map(|r| r.ack.clone()).unwrap_or_default() } else { Vec::new() };
        if steps.is_empty() {
            self.emit(Event::Ring { agent: agent.into(), state: RingState::Resting, progress: None }, out);
            self.emit(Event::GoTo { agent: agent.into(), station: home }, out);
        }
        for step in steps {
            self.push(now + step.at, Job::Step { agent: agent.into(), step, run: None });
        }
    }

    fn emit(&mut self, event: Event, out: &mut Vec<Event>) {
        self.apply(&event);
        out.push(event);
    }

    /// Updates the state from an event, whoever produced it.
    pub fn apply(&mut self, event: &Event) {
        match event {
            Event::Hello { .. }
            | Event::Action { .. }
            | Event::TerminalInput { .. }
            | Event::Message { .. }
            | Event::RunStarted { .. }
            | Event::RunFinished { .. }
            | Event::FigureDraft { .. }
            | Event::ToolResult { .. }
            | Event::NextWake { .. }
            | Event::CapabilityRequest { .. }
            | Event::CapabilitiesChanged { .. }
            | Event::IndexResults { .. }
            | Event::Imported { .. }
            | Event::Account { .. }
            | Event::InterviewStep { .. }
            | Event::OwnProfile { .. }
            | Event::Social { .. }
            | Event::Feed { .. }
            | Event::PostDrafts { .. }
            | Event::NewsDigest { .. }
            | Event::TemplateAdded { .. }
            | Event::Devices { .. }
            | Event::DeviceParam { .. }
            | Event::DeviceCapabilityRequest { .. }
            | Event::DeviceCapabilitiesChanged { .. }
            | Event::DeviceActionResult { .. }
            | Event::Graveyard { .. }
            | Event::GraveyardResult { .. }
            | Event::Answer { .. }
            | Event::AppRequest { .. }
            | Event::ConfigProposal { .. }
            | Event::ConfigResult { .. } => {}
            Event::AgentSpawned { agent, name, system } => {
                let state = self
                    .agents
                    .entry(agent.clone())
                    .or_insert_with(|| AgentState::new(name.clone(), *system, ARRIVAL_STATION.into(), true));
                state.name = name.clone();
                state.system = *system;
            }
            Event::AgentRemoved { agent } => {
                self.agents.remove(agent);
            }
            Event::Ring { agent, state, progress } => {
                if let Some(a) = self.agents.get_mut(agent) {
                    a.ring = *state;
                    a.progress = *progress;
                }
            }
            Event::GoTo { agent, station } => {
                if let Some(a) = self.agents.get_mut(agent) {
                    a.station = station.clone();
                }
            }
            Event::Activity { agent, text } => {
                if let Some(a) = self.agents.get_mut(agent) {
                    a.activity = text.clone();
                }
            }
            Event::Param { agent, name, value } => {
                if let Some(a) = self.agents.get_mut(agent) {
                    a.params.insert(name.clone(), value.clone());
                }
            }
        }
    }
}

fn state_of(agent: &AgentDir) -> AgentState {
    AgentState::new(agent.config.name.clone(), agent.config.capabilities.system, agent.config.home.clone(), is_mock(&agent.config))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scenario::tests::test_agents;
    use serde_json::json;

    fn engine(scenario: &str) -> Engine {
        Engine::new(Scenario::parse(scenario).unwrap(), &test_agents())
    }

    fn activities(events: &[Event]) -> Vec<String> {
        events
            .iter()
            .filter_map(|e| match e {
                Event::Activity { agent, text } => Some(format!("{agent}: {text}")),
                _ => None,
            })
            .collect()
    }

    const WREN_RUNS: &str = r#"
[runs.wren]
important_every = 2

[[runs.wren.step]]
go_to = "mailbox"
ring = "working"
activity = "checking mail"

[[runs.wren.quiet]]
at = 6
activity = "no new mail"
ring = "resting"
go_to = "home"

[[runs.wren.important]]
at = 6
param = { letter = true }
ring = "needs_you"
activity = "mail from the tax office"

[[runs.wren.ack]]
param = { letter = false }
ring = "resting"
go_to = "home"
"#;

    #[test]
    fn agents_start_resting_at_home() {
        let e = engine("");
        let ember = e.agent("ember").unwrap();
        assert_eq!((ember.station.as_str(), ember.ring, ember.activity.as_str()), ("workbench", RingState::Resting, ""));
        assert!(e.agent("rivet").unwrap().system);
    }

    #[test]
    fn timeline_plays_in_order_and_loops() {
        let mut e = engine(
            r#"
loop_seconds = 10
[[step]]
at = 5
agent = "ember"
activity = "second"
[[step]]
at = 1
agent = "ember"
activity = "first"
[[step]]
at = 5
agent = "rivet"
activity = "third"
"#,
        );
        assert!(e.advance(0.5).is_empty());
        assert_eq!(activities(&e.advance(5.0)), ["ember: first", "ember: second", "rivet: third"]);
        assert!(e.advance(10.0).is_empty());
        assert_eq!(activities(&e.advance(11.0)), ["ember: first"]);
        assert_eq!(activities(&e.advance(19.9)), ["ember: second", "rivet: third"]);
        // A long stall skips the missed loops instead of replaying them.
        assert!(e.advance(1000.0).is_empty());
        assert_eq!(activities(&e.advance(1005.0)), ["ember: first", "ember: second", "rivet: third"]);
    }

    #[test]
    fn timeline_without_loop_plays_once() {
        let mut e = engine("[[step]]\nat = 1\nagent = \"ember\"\nactivity = \"once\"");
        assert_eq!(activities(&e.advance(2.0)).len(), 1);
        assert!(e.advance(1000.0).is_empty());
        assert_eq!(e.next_due(), Some(40.0 * 26.0), "only wren's wakes stay queued");
    }

    #[test]
    fn steps_emit_every_field() {
        let mut e = engine(
            r#"
[[step]]
agent = "ember"
ring = "working"
progress = 0.25
go_to = "desk"
activity = "batch 3 of 5"
param = { lamp = true }
action = { tool = "terminal.run", summary = "make bench" }
type = "make\n"
[[step]]
at = 1
agent = "ember"
progress = 0.5
[[step]]
at = 2
agent = "ember"
ring = "needs_you"
"#,
        );
        let ember = || "ember".to_string();
        assert_eq!(
            e.advance(0.0),
            [
                Event::Ring { agent: ember(), state: RingState::Working, progress: Some(0.25) },
                Event::GoTo { agent: ember(), station: "desk".into() },
                Event::Activity { agent: ember(), text: "batch 3 of 5".into() },
                Event::Param { agent: ember(), name: "lamp".into(), value: json!(true) },
                Event::Action { agent: ember(), tool: "terminal.run".into(), summary: "make bench".into(), target: None },
                Event::TerminalInput { agent: ember(), data: "ma".into() },
            ]
        );
        let typed: Vec<Event> = e.advance(0.2);
        assert_eq!(
            typed,
            [Event::TerminalInput { agent: ember(), data: "ke".into() }, Event::TerminalInput { agent: ember(), data: "\n".into() }]
        );
        assert_eq!(e.advance(1.0), [Event::Ring { agent: ember(), state: RingState::Working, progress: Some(0.5) }]);
        assert_eq!(e.advance(2.0), [Event::Ring { agent: ember(), state: RingState::NeedsYou, progress: None }]);
    }

    #[test]
    fn snapshot_lists_agents_then_their_state() {
        let mut e = engine(
            r#"
[[step]]
agent = "ember"
ring = "working"
progress = 0.5
go_to = "desk"
activity = "batch 3 of 5"
param = { lamp = true, count = 3 }
"#,
        );
        e.advance(0.0);
        let s = |v: &str| v.to_string();
        assert_eq!(
            e.snapshot(),
            [
                Event::AgentSpawned { agent: s("ember"), name: s("Ember"), system: false },
                Event::AgentSpawned { agent: s("rivet"), name: s("Rivet"), system: true },
                Event::AgentSpawned { agent: s("wren"), name: s("Wren"), system: false },
                Event::Ring { agent: s("ember"), state: RingState::Working, progress: Some(0.5) },
                Event::GoTo { agent: s("ember"), station: s("desk") },
                Event::Activity { agent: s("ember"), text: s("batch 3 of 5") },
                Event::Param { agent: s("ember"), name: s("count"), value: json!(3) },
                Event::Param { agent: s("ember"), name: s("lamp"), value: json!(true) },
                Event::Ring { agent: s("rivet"), state: RingState::Resting, progress: None },
                Event::GoTo { agent: s("rivet"), station: s("router_cabinet") },
                Event::Activity { agent: s("rivet"), text: s("") },
                Event::Ring { agent: s("wren"), state: RingState::Resting, progress: None },
                Event::GoTo { agent: s("wren"), station: s("window") },
                Event::Activity { agent: s("wren"), text: s("") },
            ]
        );
    }

    #[test]
    fn schedule_alternates_quiet_and_important_runs() {
        let mut e = engine(WREN_RUNS);
        assert!(e.advance(39.9).is_empty());
        assert_eq!(activities(&e.advance(40.0)), ["wren: checking mail"]);
        assert_eq!(e.agent("wren").unwrap().station, "mailbox");
        assert_eq!(activities(&e.advance(46.0)), ["wren: no new mail"]);
        let wren = e.agent("wren").unwrap();
        assert_eq!((wren.ring, wren.station.as_str()), (RingState::Resting, "window"));

        assert_eq!(activities(&e.advance(86.0)), ["wren: checking mail", "wren: mail from the tax office"]);
        let wren = e.agent("wren").unwrap();
        assert_eq!((wren.ring, wren.params.get("letter")), (RingState::NeedsYou, Some(&json!(true))));

        // The third wake comes while wren still waits for you, so it is skipped.
        assert!(e.advance(130.0).is_empty());
        let acked = e.ack("wren", 130.0);
        assert!(acked.contains(&Event::Param { agent: "wren".into(), name: "letter".into(), value: json!(false) }));
        assert!(acked.contains(&Event::GoTo { agent: "wren".into(), station: "window".into() }));
        assert_eq!(e.agent("wren").unwrap().ring, RingState::Resting);

        // The next run is run 3, a quiet one.
        assert_eq!(activities(&e.advance(166.0)), ["wren: checking mail", "wren: no new mail"]);
        assert_eq!(activities(&e.advance(206.0)), ["wren: checking mail", "wren: mail from the tax office"]);
    }

    #[test]
    fn first_after_moves_the_first_wake() {
        let mut e = engine(&WREN_RUNS.replace("important_every = 2", "important_every = 2\nfirst_after = 5"));
        assert_eq!(activities(&e.advance(5.0)), ["wren: checking mail"]);
        assert_eq!(activities(&e.advance(45.0)), ["wren: no new mail", "wren: checking mail"]);
    }

    #[test]
    fn auto_ack_plays_the_ack_steps() {
        let scenario = WREN_RUNS.replace("important_every = 2", "important_every = 1\nauto_ack_after = 25");
        let mut e = engine(&scenario);
        e.advance(46.0);
        assert_eq!(e.agent("wren").unwrap().ring, RingState::NeedsYou);
        assert!(e.advance(70.9).is_empty());
        let events = e.advance(71.0);
        assert!(events.contains(&Event::Param { agent: "wren".into(), name: "letter".into(), value: json!(false) }));
        let wren = e.agent("wren").unwrap();
        assert_eq!((wren.ring, wren.station.as_str()), (RingState::Resting, "window"));
        // The next wake is not skipped because the run ended with the ack.
        assert_eq!(activities(&e.advance(80.0)), ["wren: checking mail"]);
    }

    #[test]
    fn ack_without_ack_steps_rests_at_home() {
        let mut e = engine("[[step]]\nagent = \"ember\"\nring = \"needs_you\"\ngo_to = \"desk\"");
        e.advance(0.0);
        assert_eq!(
            e.ack("ember", 1.0),
            [
                Event::Ring { agent: "ember".into(), state: RingState::Resting, progress: None },
                Event::GoTo { agent: "ember".into(), station: "workbench".into() },
            ]
        );
        assert!(e.ack("ember", 2.0).is_empty(), "a second ack finds nobody waiting");
        assert!(e.ack("ghost", 2.0).is_empty());
    }

    #[test]
    fn ack_cancels_the_rest_of_a_run() {
        let scenario = WREN_RUNS.replace("important_every = 2", "important_every = 1")
            + "\n[[runs.wren.important]]\nat = 20\nactivity = \"too late\"\n";
        let mut e = engine(&scenario);
        e.advance(46.0);
        e.ack("wren", 47.0);
        assert!(activities(&e.advance(79.0)).is_empty());
    }

    #[test]
    fn real_agents_ignore_the_scenario() {
        let mut agents = test_agents();
        agents[2].config.runtime =
            Some(meerora_protocol::RuntimeConfig { kind: meerora_protocol::RuntimeKind::ClaudeCli, model: None, url: None });
        let scenario = Scenario::parse(&format!("[[step]]\nagent = \"wren\"\nactivity = \"x\"\n{WREN_RUNS}")).unwrap();
        let mut e = Engine::new(scenario, &agents);
        assert!(e.advance(100.0).is_empty(), "no timeline step and no scheduled run");
        e.apply(&Event::Ring { agent: "wren".into(), state: RingState::NeedsYou, progress: None });
        let acked = e.ack("wren", 101.0);
        assert_eq!(acked.len(), 2, "the default ack, not the scenario's ack steps: {acked:?}");
    }

    #[test]
    fn agents_come_and_go() {
        let mut e = engine("");
        let quill = crate::scenario::tests::agent("quill", "name = \"Quill\"\nhome = \"desk\"");
        e.add_agent(&quill, 0.0);
        assert_eq!(e.snapshot_agent("quill")[0], Event::AgentSpawned { agent: "quill".into(), name: "Quill".into(), system: false });
        let mut renamed = quill.clone();
        renamed.config.name = "Quill II".into();
        assert!(e.update_agent(&renamed));
        assert!(!e.update_agent(&renamed));
        e.remove_agent("quill");
        assert!(e.agent("quill").is_none());
        assert_eq!(e.snapshot().iter().filter(|ev| matches!(ev, Event::AgentSpawned { .. })).count(), 3);
    }

    #[test]
    fn injected_events_change_the_state() {
        let mut e = engine("");
        let go = Event::GoTo { agent: "ember".into(), station: "door".into() };
        assert_eq!(e.inject(go.clone()), Some(go));
        assert_eq!(e.agent("ember").unwrap().station, "door");
        assert_eq!(e.inject(Event::Hello { version: 1, daemon: "x".into() }), None);

        e.inject(Event::AgentSpawned { agent: "mira".into(), name: "Mira".into(), system: false });
        assert_eq!(e.agent("mira").unwrap().station, "door");
        e.inject(Event::AgentRemoved { agent: "ember".into() });
        assert!(e.agent("ember").is_none());
        assert!(!e.snapshot().iter().any(|ev| ev.agent() == Some("ember")));
    }
}
