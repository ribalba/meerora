//! The graveyard: an agent retires reversibly and can come back.
//!
//! Retire moves the agent's directory with a rename to `graveyard/<id>/`, or
//! to `<id>-2` and so on when that name is taken, so git, memory and logs
//! come along. Only then does the agent stop: its run and queue end the way a
//! cancel ends them, which also denies its open capability requests, its
//! schedule ends with it, and it leaves the engine and the agent list
//! (`AgentRemoved`). `retired.json` beside its files keeps the time, the
//! name, the colour, the last activity line and the run count. Its CLI homes
//! below `runtimes/` stay where they are, so a restore with the same id
//! continues its conversations. The helper (`role = "helper"`, helper.rs)
//! never retires, and Retire answers why.
//!
//! Restore moves the directory back to `agents/<id>/`, or `<id>-2` and so on
//! when the id is taken again, removes `retired.json` and then takes the path
//! of a new agent directory, as `ReloadAgents` does. The agent appears at its
//! home station and its schedule resumes. `ReloadAgents` itself reads only
//! `agents/`, so it never brings a retired agent back.
//!
//! DeleteRetired moves a graveyard directory to the system trash with `gio
//! trash`. Without gio, or when gio fails, it refuses with a message that
//! names the path to remove by hand. Nothing here deletes anything for good.
//!
//! Before it leaves, the agent walks to the graveyard station (D9). The
//! retirement itself happens at once and completely: the directory moves,
//! the run stops and the agent leaves the daemon, so nothing that arrives
//! during the walk finds it half retired. Only what the clients see waits.
//! The daemon sends `GoTo graveyard` first and `AgentRemoved` with the new
//! graveyard list once the app reports the figure there (`Arrived`) or after
//! [`WALK_LIMIT`]. A restore during the walk calls it off, and the figure
//! turns back home. The daemon drops scenario and injected events for an
//! agent on its way, so nothing sends it elsewhere.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use meerora_protocol::{
    AgentDir, Command, Event, GRAVEYARD_DIR, GraveyardAction, PROTOCOL_VERSION, RETIRED_FILE, RUNS_LOG, RetiredAgent, RetiredFile,
    RingState, decode_event, encode, load_agent, load_graveyard,
};
use tracing::{info, warn};

use crate::daemon::{Daemon, Outgoing};
use crate::runs::now_ms;
use crate::scenario::Scenario;
use crate::store::Store;

/// Moves a directory to the system trash. Tests pass a fake.
pub trait Trash: Send {
    fn trash(&self, path: &Path) -> Result<(), TrashError>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TrashError {
    /// No trash tool is installed.
    Missing,
    /// The tool ran and refused, with its reason.
    Failed(String),
}

/// `gio trash -- <dir>` from GLib, the trash of the desktop.
pub struct Gio;

impl Trash for Gio {
    fn trash(&self, path: &Path) -> Result<(), TrashError> {
        let output = std::process::Command::new("gio").arg("trash").arg("--").arg(path).stdin(Stdio::null()).output();
        match output {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(TrashError::Missing),
            Err(e) => Err(TrashError::Failed(format!("gio did not start ({e})"))),
            Ok(o) if o.status.success() => Ok(()),
            Ok(o) => {
                let why = String::from_utf8_lossy(&o.stderr).trim().to_string();
                Err(TrashError::Failed(if why.is_empty() { format!("gio trash ended with {}", o.status) } else { why }))
            }
        }
    }
}

/// The station retired agents walk to before they leave the space.
pub const STATION: &str = "graveyard";
/// How long a retired agent may walk before it leaves the space anyway, for
/// clients that never report its arrival. The longest walk across a room at
/// 120 cm/s takes about 12 s.
pub const WALK_LIMIT: Duration = Duration::from_secs(15);

/// What the daemon keeps for the graveyard.
pub(crate) struct Graveyard {
    pub trash: Box<dyn Trash>,
    /// Retired agents whose figures still walk to the graveyard.
    pub walks: Vec<Walk>,
    /// [`WALK_LIMIT`], shorter in tests.
    pub walk_limit: Duration,
}

/// A retired agent on its way to the graveyard. Its directory is in the
/// graveyard already.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Walk {
    pub agent: String,
    pub grave: String,
    pub until: Instant,
}

impl Default for Graveyard {
    fn default() -> Self {
        Self { trash: Box::new(Gio), walks: Vec::new(), walk_limit: WALK_LIMIT }
    }
}

impl Daemon {
    fn graveyard_dir(&self) -> PathBuf {
        self.opts.data_dir.join(GRAVEYARD_DIR)
    }

    /// The complete graveyard, for the greeting and after every change.
    pub(crate) fn graveyard_event(&self) -> Event {
        Event::Graveyard { agents: load_graveyard(&self.opts.data_dir) }
    }

    /// [`meerora_protocol::Command::Retire`] from `client`. The agent walks
    /// to the graveyard station first when the space has one.
    pub(crate) fn retire(&mut self, client: u64, id: &str) {
        if let Some(walk) = self.graveyard.walks.iter().find(|w| w.agent == id) {
            // A second click while it walks: it is retired already.
            let grave = walk.grave.clone();
            return self.answer(client, GraveyardAction::Retire, id, Ok(Some(grave)));
        }
        let walk = self.agents.contains_key(id) && self.opts.stations.iter().any(|s| s == STATION);
        let result = self.move_to_graveyard(id, walk);
        let grave = result.as_ref().ok().cloned();
        self.answer(client, GraveyardAction::Retire, id, result.map(Some));
        match grave {
            Some(grave) if walk => {
                let until = Instant::now() + self.graveyard.walk_limit;
                info!("{id} walks to the graveyard");
                self.graveyard.walks.push(Walk { agent: id.into(), grave, until });
            }
            Some(_) => {
                let event = self.graveyard_event();
                self.out.push(Outgoing::All(event));
            }
            None => {}
        }
    }

    /// Whether the agent is on its way to the graveyard.
    pub(crate) fn walking(&self, agent: &str) -> bool {
        self.graveyard.walks.iter().any(|w| w.agent == agent)
    }

    /// [`meerora_protocol::Command::Arrived`]: a figure reached a station.
    pub(crate) fn arrived(&mut self, agent: &str, station: &str) {
        if station != STATION {
            return;
        }
        if let Some(i) = self.graveyard.walks.iter().position(|w| w.agent == agent) {
            let walk = self.graveyard.walks.remove(i);
            self.end_walk(&walk.agent);
        }
    }

    /// Ends the walks that took too long.
    pub(crate) fn tick_walks(&mut self, now: Instant) {
        let (late, walking): (Vec<Walk>, Vec<Walk>) = std::mem::take(&mut self.graveyard.walks).into_iter().partition(|w| w.until <= now);
        self.graveyard.walks = walking;
        for walk in late {
            info!("{} did not report its arrival at the graveyard in time", walk.agent);
            self.end_walk(&walk.agent);
        }
    }

    /// How long until the next walk ends at the latest.
    pub(crate) fn walk_wait(&self, now: Instant) -> Option<Duration> {
        self.graveyard.walks.iter().map(|w| w.until.saturating_duration_since(now)).min()
    }

    /// The figure is at the graveyard: it leaves, and its tombstone appears.
    fn end_walk(&mut self, agent: &str) {
        self.out.push(Outgoing::All(Event::AgentRemoved { agent: agent.into() }));
        let event = self.graveyard_event();
        self.out.push(Outgoing::All(event));
    }

    /// Moves the directory first and stops the agent once it is safe in the
    /// graveyard, so a failed move changes nothing. Returns the graveyard id.
    /// With `walk` the clients see the agent walk to the graveyard station,
    /// and [`Daemon::end_walk`] takes it out of their space later.
    fn move_to_graveyard(&mut self, id: &str, walk: bool) -> Result<String, String> {
        let Some(agent) = self.agents.get(id) else { return Err(format!("There is no agent {id} in the space.")) };
        let (name, color, from) = (agent.dir.config.name.clone(), agent.dir.config.color.clone(), agent.dir.path.clone());
        // The helper is permanent (helper.rs).
        if crate::helper::is_helper(&agent.dir.config) {
            return Err(format!("{name} is meerora's helper and always stays in the space, so it cannot move to the graveyard."));
        }
        let last_activity = self.engine.agent(id).map(|s| s.activity.trim().to_string()).filter(|a| !a.is_empty());
        let runs = run_count(&from);
        let graveyard = self.graveyard_dir();
        std::fs::create_dir_all(&graveyard)
            .map_err(|e| format!("{name} stays in the space, because {} cannot be created ({e}).", graveyard.display()))?;
        let grave = free_name(id, |n| graveyard.join(n).exists());
        let to = graveyard.join(&grave);
        std::fs::rename(&from, &to)
            .map_err(|e| format!("{name} stays in the space, because {} cannot be moved to {} ({e}).", from.display(), to.display()))?;
        info!("{id} moved to {}", to.display());

        self.deny_requests(id, "the agent moved to the graveyard");
        self.end_once_grants(id, "the agent moved to the graveyard");
        if walk {
            // It walks resting, so neither the front row (D3) nor its tools hold it back.
            if self.engine.agent(id).is_some_and(|s| s.ring != RingState::Resting) {
                self.out.push(Outgoing::All(Event::Ring { agent: id.into(), state: RingState::Resting, progress: None }));
            }
            self.out.push(Outgoing::All(Event::Activity { agent: id.into(), text: "on the way to the graveyard".into() }));
            self.out.push(Outgoing::All(Event::GoTo { agent: id.into(), station: STATION.into() }));
            self.leave_quietly(id);
        } else {
            self.remove_agent(id);
        }
        self.devices.rescan_soon();
        calm(&to);
        let file = RetiredFile { agent: id.to_string(), name, color, retired_at: now_ms(), last_activity, runs };
        if let Err(e) = write_retired(&to, &file) {
            // The graveyard still lists it from its agent.toml.
            warn!("cannot write {} ({e})", to.join(RETIRED_FILE).display());
        }
        Ok(grave)
    }

    /// Takes an agent out of the daemon like [`Daemon::remove_agent`], but
    /// leaves its figure to the clients until its walk ends.
    fn leave_quietly(&mut self, id: &str) {
        self.cancel(id);
        self.agents.remove(id);
        self.engine.remove_agent(id);
    }

    /// [`meerora_protocol::Command::Restore`] from `client`. The answer goes
    /// out before the agent's events, so the client that asked knows the id
    /// when they arrive. A restore while the agent still walks to the
    /// graveyard calls the walk off, and the figure turns back home.
    pub(crate) fn restore_retired(&mut self, client: u64, grave: &str) {
        match self.move_back(grave) {
            Ok(dir) => {
                if let Some(i) = self.graveyard.walks.iter().position(|w| w.grave == grave) {
                    let walk = self.graveyard.walks.remove(i);
                    info!("{} turns back on its way to the graveyard", walk.agent);
                    if walk.agent != dir.id {
                        // It comes back under another id, so the walking figure leaves.
                        self.out.push(Outgoing::All(Event::AgentRemoved { agent: walk.agent }));
                    }
                }
                self.answer(client, GraveyardAction::Restore, grave, Ok(Some(dir.id.clone())));
                // The path of a new agent directory, as in ReloadAgents.
                self.add_agent(dir);
                self.devices.rescan_soon();
                let event = self.graveyard_event();
                self.out.push(Outgoing::All(event));
            }
            Err(e) => self.answer(client, GraveyardAction::Restore, grave, Err(e)),
        }
    }

    fn move_back(&mut self, grave: &str) -> Result<AgentDir, String> {
        check_grave(grave)?;
        let from = self.graveyard_dir().join(grave);
        if !from.is_dir() {
            return Err(format!("There is no {grave} in the graveyard."));
        }
        let file = read_retired(&from);
        let wanted = file.as_ref().map(|f| f.agent.clone()).filter(|a| check_grave(a).is_ok()).unwrap_or_else(|| grave.to_string());
        let name = file.as_ref().map_or_else(|| grave.to_string(), |f| f.name.clone());
        // A broken agent.toml would leave a directory in agents/ that does not load.
        load_agent(&from).map_err(|e| {
            format!("{name} cannot come back, because its agent.toml does not load ({e}). Fix it in {} first.", from.display())
        })?;
        let agents = self.opts.data_dir.join("agents");
        std::fs::create_dir_all(&agents)
            .map_err(|e| format!("{name} cannot come back, because {} cannot be created ({e}).", agents.display()))?;
        let id = free_name(&wanted, |n| agents.join(n).exists() || self.agents.contains_key(n));
        let to = agents.join(&id);
        std::fs::rename(&from, &to)
            .map_err(|e| format!("{name} cannot come back, because {} cannot be moved to {} ({e}).", from.display(), to.display()))?;
        info!("{grave} came back from the graveyard as {id}");
        match std::fs::remove_file(to.join(RETIRED_FILE)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => warn!("cannot remove {} ({e})", to.join(RETIRED_FILE).display()),
            _ => {}
        }
        if id != wanted {
            // The CLI homes belong to the old id, so the conversation starts anew.
            forget_session(&to);
        }
        load_agent(&to).map_err(|e| format!("{name} is back in {}, but it does not load ({e}).", to.display()))
    }

    /// [`meerora_protocol::Command::DeleteRetired`] from `client`.
    pub(crate) fn delete_retired(&mut self, client: u64, grave: &str) {
        let result = self.trash_grave(grave);
        let changed = result.is_ok();
        self.answer(client, GraveyardAction::Delete, grave, result.map(|()| None));
        if changed {
            let event = self.graveyard_event();
            self.out.push(Outgoing::All(event));
        }
    }

    fn trash_grave(&mut self, grave: &str) -> Result<(), String> {
        check_grave(grave)?;
        let path = self.graveyard_dir().join(grave);
        if !path.is_dir() {
            return Err(format!("There is no {grave} in the graveyard."));
        }
        let by_hand = format!("meerora never deletes for good, so remove {} by hand if you want it gone.", path.display());
        match self.graveyard.trash.trash(&path) {
            Ok(()) => {
                info!("{} is in the trash", path.display());
                Ok(())
            }
            Err(TrashError::Missing) => Err(format!("Moving to the trash needs gio from GLib, which is not installed. {by_hand}")),
            Err(TrashError::Failed(why)) => Err(format!("The trash refused {} ({why}). {by_hand}", path.display())),
        }
    }

    fn answer(&mut self, client: u64, action: GraveyardAction, agent: &str, result: Result<Option<String>, String>) {
        let (id, error) = match result {
            Ok(id) => (id, None),
            Err(e) => {
                info!("{action:?} of {agent} refused ({e})");
                (None, Some(e))
            }
        };
        self.out.push(Outgoing::To(client, Event::GraveyardResult { action, agent: agent.into(), id, error }));
    }
}

/// `wanted`, or `wanted-2`, `wanted-3` and so on, the first name `taken` lets through.
fn free_name(wanted: &str, taken: impl Fn(&str) -> bool) -> String {
    if !taken(wanted) {
        return wanted.to_string();
    }
    (2u32..).map(|n| format!("{wanted}-{n}")).find(|n| !taken(n)).unwrap_or_else(|| format!("{wanted}-{}", now_ms()))
}

/// A graveyard id or an agent id names one directory and nothing else.
fn check_grave(name: &str) -> Result<(), String> {
    let plain = !name.is_empty() && !name.starts_with('.') && !name.contains(['/', '\\', '\0']);
    if plain { Ok(()) } else { Err(format!("{name:?} is not a name in the graveyard.")) }
}

/// The runs in the agent's history.
fn run_count(dir: &Path) -> usize {
    std::fs::read_to_string(dir.join(RUNS_LOG)).map_or(0, |text| text.lines().filter(|l| !l.trim().is_empty()).count())
}

fn read_retired(dir: &Path) -> Option<RetiredFile> {
    serde_json::from_str(&std::fs::read_to_string(dir.join(RETIRED_FILE)).ok()?).ok()
}

fn write_retired(dir: &Path, file: &RetiredFile) -> std::io::Result<()> {
    let mut text = serde_json::to_string_pretty(file).map_err(std::io::Error::other)?;
    text.push('\n');
    // The agent's directory could hold a symlink by that name (safefs).
    meerora_protocol::safefs::replace(dir, RETIRED_FILE, text.as_bytes())
}

/// Changes `state.json` when it reads cleanly. A broken one stays as it is.
fn edit_state(dir: &Path, change: impl FnOnce(&mut Store) -> bool) {
    let Ok(text) = std::fs::read_to_string(dir.join("state.json")) else { return };
    let Ok(mut store) = serde_json::from_str::<Store>(&text) else { return };
    if change(&mut store)
        && let Err(e) = store.save(dir)
    {
        warn!("cannot write {} ({e})", dir.join("state.json").display());
    }
}

/// A retired agent comes back resting, not waiting for the user.
fn calm(dir: &Path) {
    edit_state(dir, |store| {
        let waiting = store.ring == Some(RingState::NeedsYou);
        if waiting {
            store.ring = Some(RingState::Resting);
        }
        waiting
    });
}

/// Drops the runtime session of an agent that came back under a new id.
fn forget_session(dir: &Path) {
    edit_state(dir, |store| store.session.take().is_some());
}

/// Leaves the timeline steps and runs of retired agents out of the mock
/// scenario, so a scenario that names them still validates and plays. An id
/// that an agent in the space holds again keeps its steps. Returns a warning
/// when it left anything out.
pub fn skip_retired(scenario: &mut Scenario, data_dir: &Path, agents: &[AgentDir]) -> Option<String> {
    let mut retired: Vec<String> =
        load_graveyard(data_dir).into_iter().map(|a| a.agent).filter(|id| !agents.iter().any(|a| a.id == *id)).collect();
    retired.sort();
    retired.dedup();
    let named = |id: &Option<String>| id.as_ref().is_some_and(|id| retired.contains(id));
    let before = scenario.steps.len() + scenario.runs.len();
    scenario.steps.retain(|step| !named(&step.agent));
    scenario.runs.retain(|id, _| !retired.contains(id));
    let skipped = before - scenario.steps.len() - scenario.runs.len();
    (skipped > 0).then(|| format!("the scenario steps of {} wait in the graveyard and do not play", retired.join(", ")))
}

// The command line: `meerora-agentd retire`, `restore` and `graveyard`.

const CLI_TIMEOUT: Duration = Duration::from_secs(30);

/// Sends hello and `command`, then returns the first event `answer` accepts.
fn ask_daemon(socket: &Path, command: Option<&Command>, answer: impl Fn(&Event) -> bool) -> anyhow::Result<Event> {
    let mut stream =
        UnixStream::connect(socket).with_context(|| format!("cannot connect to {}. Is meerora-agentd running?", socket.display()))?;
    stream.set_read_timeout(Some(CLI_TIMEOUT))?;
    let hello = Command::Hello { version: PROTOCOL_VERSION, client: format!("meerora-agentd {} (graveyard)", env!("CARGO_PKG_VERSION")) };
    let mut lines = encode(&hello);
    if let Some(command) = command {
        lines.push_str(&encode(command));
    }
    stream.write_all(lines.as_bytes())?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).context("no answer from the daemon")? == 0 {
            bail!("the daemon closed the connection");
        }
        if let Ok(event) = decode_event(&line)
            && answer(&event)
        {
            return Ok(event);
        }
    }
}

/// `retire ID` and `restore ID` with the command, `graveyard` without one.
pub fn run_cli(socket: &Path, command: Option<Command>, out: &mut dyn Write) -> anyhow::Result<()> {
    let (action, agent) = match &command {
        None => return list_cli(socket, out),
        Some(Command::Retire { agent }) => (GraveyardAction::Retire, agent.clone()),
        Some(Command::Restore { agent }) => (GraveyardAction::Restore, agent.clone()),
        Some(Command::DeleteRetired { agent }) => (GraveyardAction::Delete, agent.clone()),
        Some(other) => bail!("{other:?} is not a graveyard command"),
    };
    let event = ask_daemon(
        socket,
        command.as_ref(),
        |e| matches!(e, Event::GraveyardResult { action: a, agent: g, .. } if *a == action && *g == agent),
    )?;
    let Event::GraveyardResult { id, error, .. } = event else { unreachable!("ask_daemon checked the event") };
    if let Some(error) = error {
        bail!("{error}");
    }
    let id = id.unwrap_or_default();
    match action {
        GraveyardAction::Retire => writeln!(out, "{agent} is in {GRAVEYARD_DIR}/{id}. Bring it back with: meerora-agentd restore {id}")?,
        GraveyardAction::Restore => writeln!(out, "{agent} is back in the space as agents/{id}")?,
        GraveyardAction::Delete => writeln!(out, "{agent} is in the trash")?,
    }
    Ok(())
}

/// Prints one line per retired agent, the newest first.
fn list_cli(socket: &Path, out: &mut dyn Write) -> anyhow::Result<()> {
    let event = ask_daemon(socket, None, |e| matches!(e, Event::Graveyard { .. }))?;
    let Event::Graveyard { agents } = event else { unreachable!("ask_daemon checked the event") };
    if agents.is_empty() {
        writeln!(out, "The graveyard is empty.")?;
    }
    for a in &agents {
        writeln!(out, "{}", describe(a))?;
    }
    Ok(())
}

/// One line about a retired agent for `meerora-agentd graveyard`.
fn describe(a: &RetiredAgent) -> String {
    let day = jiff::Timestamp::from_millisecond(a.retired_at as i64)
        .map(|t| t.to_zoned(jiff::tz::TimeZone::system()).strftime("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_default();
    let runs = if a.runs == 1 { "1 run".to_string() } else { format!("{} runs", a.runs) };
    let was = if a.agent == a.id { String::new() } else { format!(" (was {})", a.agent) };
    let last = a.last_activity.as_deref().map(|l| format!(" · last: {l}")).unwrap_or_default();
    format!("{}  {}{was} · retired {day} · {runs}{last} · {}", a.id, a.name, a.path.display())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_count_up_and_stay_plain() {
        let taken = ["wren", "wren-2"];
        assert_eq!(free_name("wren", |n| taken.contains(&n)), "wren-3");
        assert_eq!(free_name("moth", |n| taken.contains(&n)), "moth");
        for bad in ["", ".", "..", ".hidden", "a/b", "../agents"] {
            assert!(check_grave(bad).is_err(), "{bad:?}");
        }
        assert!(check_grave("wren-2").is_ok());
    }

    #[test]
    fn the_scenario_skips_retired_agents() {
        let dir = crate::store::tests::TempDir::new("graveyard-scenario");
        let grave = dir.0.join(GRAVEYARD_DIR).join("moth");
        std::fs::create_dir_all(&grave).unwrap();
        let file =
            RetiredFile { agent: "moth".into(), name: "Moth".into(), color: "#3B404C".into(), retired_at: 1, last_activity: None, runs: 0 };
        write_retired(&grave, &file).unwrap();
        let text = "[[step]]\nat = 0\nagent = \"moth\"\ngo_to = \"door\"\n[[step]]\nat = 1\nagent = \"alpha\"\ngo_to = \"door\"\n\
                    [runs.moth]\n[[runs.moth.step]]\ngo_to = \"tree\"\n";
        let mut scenario: Scenario = toml::from_str(text).unwrap();
        let warning = skip_retired(&mut scenario, &dir.0, &[]).expect("a warning");
        assert!(warning.contains("moth"), "{warning}");
        assert_eq!(scenario.steps.len(), 1);
        assert_eq!(scenario.steps[0].agent.as_deref(), Some("alpha"));
        assert!(scenario.runs.is_empty());
        assert!(skip_retired(&mut scenario, &dir.0, &[]).is_none(), "nothing left to skip");

        // A new agent with the old id keeps the steps.
        let mut scenario: Scenario = toml::from_str(text).unwrap();
        let moth = crate::scenario::tests::agent("moth", "name = \"Moth II\"\nhome = \"door\"");
        assert!(skip_retired(&mut scenario, &dir.0, &[moth]).is_none());
        assert_eq!((scenario.steps.len(), scenario.runs.len()), (2, 1));
    }

    #[test]
    fn a_line_per_retired_agent() {
        let a = RetiredAgent {
            id: "wren-2".into(),
            agent: "wren".into(),
            name: "Wren".into(),
            color: "#D9A441".into(),
            retired_at: 0,
            figure: String::new(),
            last_activity: Some("checking mail".into()),
            runs: 1,
            path: "/d/graveyard/wren-2".into(),
        };
        let line = describe(&a);
        assert!(line.starts_with("wren-2  Wren (was wren) · retired "), "{line}");
        assert!(line.ends_with(" · 1 run · last: checking mail · /d/graveyard/wren-2"), "{line}");
    }
}
