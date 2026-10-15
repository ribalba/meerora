//! The scenario that mock agents play, and the validation of the scenario,
//! the agent directories (D21) and their schedules (D23) against the stations
//! of the space (D9). Real agents ignore the scenario.
//!
//! A scenario has a looping timeline of steps and, per scheduled agent, a run
//! definition that is played every time the agent wakes (D23). A short example
//! follows.
//!
//! ```toml
//! loop_seconds = 120
//!
//! [[step]]
//! at = 0
//! agent = "ember"
//! go_to = "workbench"
//! ring = "working"
//! progress = 0.1
//! activity = "batch 3 of 5"
//!
//! [runs.wren]
//! important_every = 2
//! auto_ack_after = 25
//!
//! [[runs.wren.step]]
//! go_to = "mailbox"
//! ring = "working"
//! activity = "checking mail"
//!
//! [[runs.wren.important]]
//! at = 6
//! param = { letter = true }
//! ring = "needs_you"
//! ```

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::Context;
use meerora_protocol::{AgentConfig, AgentDir, RingState, RuntimeKind, parse_duration};
use serde::Deserialize;

use crate::schedule::check_entry;

/// The special `go_to` value that sends an agent to its own `home` station.
pub const HOME: &str = "home";

/// A whole scenario file.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scenario {
    /// Length of one timeline loop in seconds. Without it the timeline plays once.
    #[serde(default)]
    pub loop_seconds: Option<f64>,
    /// The timeline, written as `[[step]]` tables in any order.
    #[serde(default, rename = "step")]
    pub steps: Vec<Step>,
    /// Scheduled runs keyed by agent id, written as `[runs.<agent>]`.
    #[serde(default)]
    pub runs: BTreeMap<String, Run>,
}

/// One moment of the timeline or of a run. Every field except `at` is optional
/// and a step may combine several of them.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    /// Seconds from the start of the loop, the wake time or the ack.
    #[serde(default)]
    pub at: f64,
    /// Agent id. Required on the timeline, implied by the key inside a run.
    #[serde(default)]
    pub agent: Option<String>,
    /// A station, or `home` for the agent's own home station.
    #[serde(default)]
    pub go_to: Option<String>,
    /// New ring state. Without `progress` it clears the progress.
    #[serde(default)]
    pub ring: Option<RingState>,
    /// Progress from 0 to 1. Without `ring` it keeps the current ring state.
    #[serde(default)]
    pub progress: Option<f32>,
    #[serde(default)]
    pub activity: Option<String>,
    /// Figure params (D24), sent as one `Event::Param` each.
    #[serde(default)]
    pub param: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    pub action: Option<ActionSpec>,
    /// Text typed into the agent's terminal in small chunks.
    #[serde(default, rename = "type")]
    pub typing: Option<String>,
}

/// An entry for the action log (D12).
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionSpec {
    pub tool: String,
    pub summary: String,
}

/// What a scheduled agent does when it wakes (D23).
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Run {
    /// Steps of every run, relative to the wake time.
    #[serde(default, rename = "step")]
    pub steps: Vec<Step>,
    /// Extra steps of runs that find nothing important.
    #[serde(default)]
    pub quiet: Vec<Step>,
    /// Extra steps of every `important_every`-th run.
    #[serde(default)]
    pub important: Vec<Step>,
    /// Steps played when the user acknowledges the agent.
    #[serde(default)]
    pub ack: Vec<Step>,
    /// Every n-th run plays `important` instead of `quiet`.
    #[serde(default)]
    pub important_every: Option<u32>,
    /// Plays the ack steps by itself this many seconds after a run asked for you.
    #[serde(default)]
    pub auto_ack_after: Option<f64>,
    /// Seconds until the first wake. Defaults to one interval.
    #[serde(default)]
    pub first_after: Option<f64>,
}

impl Run {
    /// Whether run number `n` (counted from 1) is an important one.
    pub fn is_important(&self, n: u64) -> bool {
        self.important_every.is_some_and(|k| k > 0 && n.is_multiple_of(u64::from(k)))
    }
}

impl Step {
    fn does_something(&self) -> bool {
        self.go_to.is_some()
            || self.ring.is_some()
            || self.progress.is_some()
            || self.activity.is_some()
            || !self.param.is_empty()
            || self.action.is_some()
            || self.typing.is_some()
    }
}

impl Scenario {
    pub fn parse(text: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(text)
    }

    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| format!("cannot read scenario {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("invalid scenario {}", path.display()))
    }
}

/// Whether the scenario drives this agent. Agents without a `[runtime]`
/// table are mocks as well.
pub fn is_mock(config: &AgentConfig) -> bool {
    config.runtime.as_ref().is_none_or(|r| r.kind == RuntimeKind::Mock)
}

/// The name of a runtime kind as written in agent.toml.
pub fn kind_name(kind: RuntimeKind) -> &'static str {
    match kind {
        RuntimeKind::ClaudeCli => "claude-cli",
        RuntimeKind::CodexCli => "codex-cli",
        RuntimeKind::Ollama => "ollama",
        RuntimeKind::Api => "api",
        RuntimeKind::External => "external",
        RuntimeKind::Mock => "mock",
    }
}

fn runtime_of(agent: &AgentDir) -> &'static str {
    agent.config.runtime.as_ref().map_or("mock", |r| kind_name(r.kind))
}

/// Wake intervals of a mock agent in seconds, from the `every` entries of its
/// schedule. Entries that do not parse are left out (validation reports them).
pub fn wake_intervals(agent: &AgentDir) -> Vec<f64> {
    agent.config.schedule.iter().filter_map(|s| s.every.as_deref().and_then(parse_duration)).map(|d| d.as_secs_f64()).collect()
}

/// Problems found by [`validate`]. Errors stop the daemon, warnings are logged.
#[derive(Debug, Default)]
pub struct Report {
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

impl Report {
    pub fn is_ok(&self) -> bool {
        self.errors.is_empty()
    }
}

/// Checks that every agent, station, schedule and number in the scenario
/// makes sense for the given agents and stations. `agent_errors` are agent
/// directories that failed to load.
pub fn validate(scenario: &Scenario, agents: &[AgentDir], agent_errors: &[String], stations: &[String]) -> Report {
    let mut report = Report::default();
    report.errors.extend(agent_errors.iter().cloned());
    let find = |id: &str| agents.iter().find(|a| a.id == id);
    let known = |station: &str| stations.iter().any(|s| s == station);

    for agent in agents {
        if !known(&agent.config.home) {
            report.errors.push(format!("agent {} has home {:?}, which is not a known station", agent.id, agent.config.home));
        }
        for (i, entry) in agent.config.schedule.iter().enumerate() {
            let at = format!("schedule #{} of agent {}", i + 1, agent.id);
            if let Err(e) = check_entry(entry) {
                report.errors.push(format!("In {at}, {e}."));
            } else if is_mock(&agent.config) && entry.every.is_none() {
                report.warnings.push(format!("{at} never wakes because mock agents only wake on `every`"));
            }
            if let Some(station) = &entry.station
                && !known(station)
            {
                report.errors.push(format!("{at} names station {station:?}, which is not a known station"));
            }
        }
    }

    match scenario.loop_seconds {
        Some(l) if !(l.is_finite() && l > 0.0) => report.errors.push(format!("loop_seconds must be above 0, not {l}")),
        _ => {}
    }

    let mut real_in_timeline = BTreeSet::new();
    for (i, step) in scenario.steps.iter().enumerate() {
        let at = format!("step #{} (at {} s)", i + 1, step.at);
        match step.agent.as_deref().map(|id| (id, find(id))) {
            None => report.errors.push(format!("{at} has no agent")),
            Some((id, None)) => report.errors.push(format!("{at} names unknown agent {id:?}")),
            Some((id, Some(agent))) if !is_mock(&agent.config) => {
                real_in_timeline.insert((id.to_string(), runtime_of(agent)));
            }
            Some(_) => {}
        }
        if let Some(l) = scenario.loop_seconds
            && step.at >= l
        {
            report.errors.push(format!("{at} must come before loop_seconds ({l})"));
        }
        check_step(&at, step, stations, &mut report);
    }
    for (id, runtime) in real_in_timeline {
        report.warnings.push(format!("the timeline steps for {id} are ignored because {id} runs on {runtime}"));
    }

    for (id, run) in &scenario.runs {
        let Some(agent) = find(id) else {
            report.errors.push(format!("runs.{id} is for unknown agent {id:?}"));
            continue;
        };
        let lists = [("step", &run.steps), ("quiet", &run.quiet), ("important", &run.important), ("ack", &run.ack)];
        for (list, steps) in lists {
            for (i, step) in steps.iter().enumerate() {
                let at = format!("runs.{id}.{list} #{} (at {} s)", i + 1, step.at);
                if let Some(other) = step.agent.as_deref()
                    && other != id
                {
                    report.errors.push(format!("{at} names agent {other:?}, but a run step cannot move another agent"));
                }
                check_step(&at, step, stations, &mut report);
            }
        }
        if run.important_every == Some(0) {
            report.errors.push(format!("runs.{id} needs an important_every of at least 1"));
        }
        if let Some(s) = run.auto_ack_after
            && !(s.is_finite() && s > 0.0)
        {
            report.errors.push(format!("runs.{id} needs an auto_ack_after above 0, not {s}"));
        }
        if let Some(s) = run.first_after
            && !(s.is_finite() && s >= 0.0)
        {
            report.errors.push(format!("runs.{id} needs a first_after of 0 or more, not {s}"));
        }
        if !is_mock(&agent.config) {
            report.warnings.push(format!("runs.{id} is ignored because {id} runs on {}", runtime_of(agent)));
            continue;
        }
        let wakes = !run.steps.is_empty() || !run.quiet.is_empty() || !run.important.is_empty();
        if wakes && wake_intervals(agent).is_empty() {
            report.warnings.push(format!("runs.{id} never plays because agent {id} has no `every` schedule in its agent.toml"));
        }
        if !run.important.is_empty() && run.important_every.is_none() {
            report.warnings.push(format!("runs.{id} has important steps that never play without important_every"));
        }
    }

    for agent in agents {
        if is_mock(&agent.config) && !wake_intervals(agent).is_empty() && !scenario.runs.contains_key(&agent.id) {
            report.warnings.push(format!("agent {} has a schedule but the scenario has no runs.{} for it", agent.id, agent.id));
        }
    }
    report
}

fn check_step(at: &str, step: &Step, stations: &[String], report: &mut Report) {
    if !(step.at.is_finite() && step.at >= 0.0) {
        report.errors.push(format!("{at} must not be negative"));
    }
    if let Some(station) = step.go_to.as_deref()
        && station != HOME
        && !stations.iter().any(|s| s == station)
    {
        report.errors.push(format!("{at} goes to unknown station {station:?}. Known are {} and {HOME}.", stations.join(", ")));
    }
    if let Some(p) = step.progress
        && !(0.0..=1.0).contains(&p)
    {
        report.errors.push(format!("{at} has progress {p}, which is not between 0 and 1"));
    }
    if step.typing.as_deref() == Some("") {
        report.errors.push(format!("{at} types nothing"));
    }
    if !step.does_something() {
        report.errors.push(format!("{at} does nothing"));
    }
}

/// A few lines that describe the scenario, printed by `meerora-agentd check`.
pub fn summary(scenario: &Scenario, agents: &[AgentDir]) -> Vec<String> {
    let mut lines = Vec::new();
    let names: Vec<String> = agents
        .iter()
        .map(|a| {
            let mut tags = Vec::new();
            if a.config.capabilities.system {
                tags.push("system");
            }
            if !is_mock(&a.config) {
                tags.push(runtime_of(a));
            }
            if tags.is_empty() { a.id.clone() } else { format!("{} ({})", a.id, tags.join(", ")) }
        })
        .collect();
    lines.push(format!("agents    {}: {}", agents.len(), names.join(", ")));
    let looping = match scenario.loop_seconds {
        Some(l) => format!("loops every {l} s"),
        None => "plays once".into(),
    };
    lines.push(format!("timeline  {} steps, {looping}", scenario.steps.len()));
    for (id, run) in &scenario.runs {
        let mut parts = Vec::new();
        if let Some(agent) = agents.iter().find(|a| &a.id == id) {
            for entry in agent.config.schedule.iter().filter(|e| e.every.is_some()) {
                let station = entry.station.as_deref().map(|s| format!(" at {s}")).unwrap_or_default();
                parts.push(format!("wakes every {}{station}", entry.every.as_deref().unwrap_or_default()));
            }
        }
        if let Some(s) = run.first_after {
            parts.push(format!("first after {s} s"));
        }
        parts.push(format!("{} steps per run", run.steps.len()));
        parts.push(format!("{} quiet", run.quiet.len()));
        match run.important_every {
            Some(k) => parts.push(format!("{} important (important_every = {k})", run.important.len())),
            None => parts.push(format!("{} important", run.important.len())),
        }
        parts.push(format!("{} ack", run.ack.len()));
        if let Some(s) = run.auto_ack_after {
            parts.push(format!("auto ack after {s} s"));
        }
        lines.push(format!("{:<9} {}", format!("runs.{id}"), parts.join(", ")));
    }
    lines
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::space::FALLBACK_STATIONS;
    use std::path::PathBuf;

    pub(crate) fn stations() -> Vec<String> {
        FALLBACK_STATIONS.iter().map(|s| s.to_string()).collect()
    }

    /// An agent directory without a file behind it.
    pub(crate) fn agent(id: &str, toml_text: &str) -> AgentDir {
        let config: AgentConfig = toml::from_str(toml_text).unwrap();
        AgentDir { id: id.into(), path: PathBuf::from(format!("/nowhere/{id}")), config }
    }

    pub(crate) fn test_agents() -> Vec<AgentDir> {
        vec![
            agent("ember", "name = \"Ember\"\nhome = \"workbench\""),
            agent("rivet", "name = \"Rivet\"\nhome = \"router_cabinet\"\n[capabilities]\nsystem = true"),
            agent(
                "wren",
                "name = \"Wren\"\nhome = \"window\"\n[[schedule]]\nevery = \"40s\"\ntask = \"Check mail.\"\nstation = \"mailbox\"",
            ),
        ]
    }

    #[test]
    fn parses_every_field() {
        let s = Scenario::parse(
            r#"
loop_seconds = 120

[[step]]
at = 3
agent = "ember"
go_to = "workbench"
ring = "working"
progress = 0.5
activity = "batch 3 of 5"
param = { letter = true, count = 2 }
action = { tool = "terminal.run", summary = "make bench" }
type = "make bench\n"

[runs.wren]
important_every = 2
auto_ack_after = 25
first_after = 8.5

[[runs.wren.step]]
go_to = "mailbox"

[[runs.wren.quiet]]
at = 6
ring = "resting"

[[runs.wren.important]]
at = 6
ring = "needs_you"

[[runs.wren.ack]]
go_to = "home"
"#,
        )
        .unwrap();
        assert_eq!(s.loop_seconds, Some(120.0));
        let step = &s.steps[0];
        assert_eq!(step.at, 3.0);
        assert_eq!(step.agent.as_deref(), Some("ember"));
        assert_eq!(step.ring, Some(RingState::Working));
        assert_eq!(step.progress, Some(0.5));
        assert_eq!(step.param["letter"], serde_json::json!(true));
        assert_eq!(step.param["count"], serde_json::json!(2));
        assert_eq!(step.action, Some(ActionSpec { tool: "terminal.run".into(), summary: "make bench".into() }));
        assert_eq!(step.typing.as_deref(), Some("make bench\n"));
        let run = &s.runs["wren"];
        assert_eq!((run.steps.len(), run.quiet.len(), run.important.len(), run.ack.len()), (1, 1, 1, 1));
        assert_eq!(run.important[0].ring, Some(RingState::NeedsYou));
        assert_eq!(run.ack[0].at, 0.0);
        assert_eq!((run.important_every, run.auto_ack_after, run.first_after), (Some(2), Some(25.0), Some(8.5)));
        assert!(validate(&s, &test_agents(), &[], &stations()).is_ok());
    }

    #[test]
    fn rejects_unknown_fields_and_ring_states() {
        assert!(Scenario::parse("[[step]]\nat = 1\nagent = \"ember\"\ngoto = \"desk\"").is_err());
        assert!(Scenario::parse("[[step]]\nagent = \"ember\"\nring = \"busy\"").is_err());
        assert!(Scenario::parse("loop = 3").is_err());
    }

    #[test]
    fn important_runs_alternate() {
        let run = Run { important_every: Some(2), ..Run::default() };
        let pattern: Vec<bool> = (1..=4).map(|n| run.is_important(n)).collect();
        assert_eq!(pattern, [false, true, false, true]);
        assert!(!Run::default().is_important(2));
    }

    #[test]
    fn validation_finds_problems() {
        let s = Scenario::parse(
            r#"
loop_seconds = 10
[[step]]
at = 12
agent = "ember"
go_to = "kitchen"
progress = 1.5
[[step]]
agent = "nobody"
activity = "x"
[[step]]
agent = "rivet"
[[runs.ember.step]]
agent = "rivet"
go_to = "home"
[[runs.ghost.ack]]
ring = "resting"
"#,
        )
        .unwrap();
        let report = validate(&s, &test_agents(), &["agents/broken/agent.toml: oops".into()], &stations());
        let all = report.errors.join("\n");
        for needle in [
            "must come before loop_seconds",
            "unknown station \"kitchen\"",
            "which is not between 0 and 1",
            "unknown agent \"nobody\"",
            "step #3 (at 0 s) does nothing",
            "cannot move another agent",
            "runs.ghost is for unknown agent",
            "oops",
        ] {
            assert!(all.contains(needle), "missing {needle:?} in:\n{all}");
        }
        let warnings = report.warnings.join("\n");
        assert!(warnings.contains("runs.ember never plays because agent ember has no `every` schedule"), "{warnings}");
        assert!(warnings.contains("agent wren has a schedule"), "{warnings}");
    }

    #[test]
    fn unknown_home_or_schedule_station_is_an_error() {
        let agents = vec![agent("x", "name = \"X\"\nhome = \"attic\"\n[[schedule]]\nevery = \"soon\"\ntask = \"t\"\nstation = \"cellar\"")];
        let report = validate(&Scenario::default(), &agents, &[], &stations());
        let all = report.errors.join("\n");
        assert!(all.contains("home \"attic\""), "{all}");
        assert!(all.contains("station \"cellar\""), "{all}");
        assert!(all.contains("In schedule #1 of agent x, every = \"soon\" is not a duration"), "{all}");
    }

    #[test]
    fn schedules_with_at_and_quiet_hours() {
        let agents = vec![
            agent(
                "a",
                "name = \"A\"\nhome = \"desk\"\n[runtime]\nkind = \"claude-cli\"\n[[schedule]]\nat = \"07:00, 18:30\"\ntask = \"t\"\nquiet_hours = \"22:00-07:00\"",
            ),
            agent(
                "b",
                "name = \"B\"\nhome = \"desk\"\n[runtime]\nkind = \"ollama\"\n[[schedule]]\nat = \"0 7 * * *\"\ntask = \"t\"\nquiet_hours = \"late\"",
            ),
            agent("c", "name = \"C\"\nhome = \"desk\"\n[[schedule]]\nat = \"07:00\"\ntask = \"t\""),
        ];
        let report = validate(&Scenario::default(), &agents, &[], &stations());
        let all = report.errors.join("\n");
        assert_eq!(report.errors.len(), 1, "{all}");
        assert!(all.contains("schedule #1 of agent b") && all.contains("cron syntax"), "{all}");
        let warnings = report.warnings.join("\n");
        assert!(warnings.contains("schedule #1 of agent c never wakes because mock agents only wake on `every`"), "{warnings}");
    }

    #[test]
    fn steps_for_real_agents_are_ignored_with_a_warning() {
        let mut agents = test_agents();
        agents[0].config.runtime = Some(meerora_protocol::RuntimeConfig { kind: RuntimeKind::ClaudeCli, model: None, url: None });
        let s = Scenario::parse("[[step]]\nagent = \"ember\"\nactivity = \"a\"\n[[step]]\nagent = \"ember\"\nactivity = \"b\"").unwrap();
        let report = validate(&s, &agents, &[], &stations());
        assert!(report.is_ok());
        assert!(report.warnings.contains(&"the timeline steps for ember are ignored because ember runs on claude-cli".to_string()));
        assert_eq!(report.warnings.iter().filter(|w| w.contains("ember")).count(), 1, "one warning per agent");
        assert!(!is_mock(&agents[0].config) && is_mock(&agents[1].config));
    }
}
