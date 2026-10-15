//! The helper's configuration tools (`role = "helper"`, helper.rs).
//!
//! `config_read` returns meerora's configuration as text: `meerora.toml`,
//! the agents, one `agent.toml`, the room with its devices, `commands.toml`,
//! the end of `audit.jsonl`, what `meerora-agentd check` reports and the
//! sections of the README, meerora's manual (helper.rs). The
//! targets are fixed, so nothing else of the data dir can be read. The index
//! token (`index-account.json`) and the CLI homes in `runtimes/` are never
//! read, and a token that shows up in a text anyway is hidden.
//!
//! `config_change` proposes the whole new content of `meerora.toml`,
//! `commands.toml`, an existing agent's `agent.toml` (`agent:<id>`) or a new
//! agent (`new_agent:<id>`, an `agent.toml` and an optional figure script).
//! The daemon checks it the way meerora reads it (TOML, `Settings`,
//! `AgentConfig`, stations, schedules, workspace folders, roles) and refuses
//! it with a readable error. A proposal that passes goes to the app as
//! [`Event::ConfigProposal`] with a unified diff, and the tool call waits for
//! the user's decision like `request_capability`. Granting capabilities this
//! way is allowed only because the user sees the diff, and the card
//! highlights the lines that change what an agent may do.
//!
//! Only after Apply does the daemon write the files. Leading comment lines
//! of the old file stay when the new content has none, like the builder
//! keeps them. A file that changed since the proposal is not overwritten.
//! The daemon then reloads what changed and logs the decision in
//! `audit.jsonl`. Discard writes nothing. A proposal closes without a write
//! when the helper's run is cancelled, when the helper leaves or after
//! [`PROPOSAL_WAIT`], the time the bridge waits for the answer.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use meerora_protocol::{AgentConfig, AgentRole, Event, RingState, RuntimeKind, Settings, ToolCall, load_graveyard};
use serde::{Deserialize, Serialize};
use similar::{Algorithm, ChangeTag, TextDiff};
use tracing::{info, warn};

use crate::daemon::{Daemon, Msg, Outgoing};
use crate::helper::{self, is_helper};
use crate::runs::cut;

/// Most bytes of a proposed file or figure script.
pub const CONTENT_MAX: usize = 64 * 1024;
/// Longest reason, in characters.
const REASON_MAX: usize = 300;
/// How many lines of `audit.jsonl` `config_read audit` returns.
pub const AUDIT_LINES: usize = 50;
/// How long a proposal waits for the user. The bridge gives up after the
/// same ten minutes (meerora-mcp `DECISION_TIMEOUT`).
pub const PROPOSAL_WAIT: Duration = Duration::from_secs(10 * 60);
/// What `config_read` takes, for its error message.
const READ_TARGETS: &str = "meerora.toml, agents, agent:<id>, rooms, commands, audit, check, readme or readme:<words>";

/// The open proposals of the helper.
#[derive(Default)]
pub(crate) struct Proposals {
    open: Vec<Proposal>,
    /// [`PROPOSAL_WAIT`], shorter in tests.
    wait: Option<Duration>,
}

impl Proposals {
    #[cfg(test)]
    pub fn set_wait(&mut self, wait: Duration) {
        self.wait = Some(wait);
    }
}

/// A change the user has not decided yet.
struct Proposal {
    id: String,
    agent: String,
    target: Target,
    reason: String,
    diff: String,
    plan: Plan,
    /// The bridges waiting for the decision, as client and tool call id.
    waiters: Vec<(u64, String)>,
    until: Instant,
}

/// What a `config_change` changes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Target {
    Settings,
    Commands,
    Agent(String),
    NewAgent(String),
}

impl Target {
    pub fn parse(text: &str) -> Result<Target, String> {
        let text = text.trim();
        let id_ok = |id: &str| {
            if agent_id_ok(id) {
                Ok(id.to_string())
            } else {
                Err(format!("{id:?} is not an agent id. Ids are lowercase letters, digits, - and _, starting with a letter."))
            }
        };
        match text {
            "meerora.toml" => Ok(Target::Settings),
            "commands.toml" | "commands" => Ok(Target::Commands),
            _ => match (text.strip_prefix("agent:"), text.strip_prefix("new_agent:")) {
                (Some(id), _) => id_ok(id.trim()).map(Target::Agent),
                (_, Some(id)) => id_ok(id.trim()).map(Target::NewAgent),
                _ => Err(format!(
                    "{text:?} cannot be changed. config_change takes meerora.toml, commands.toml, agent:<id> or new_agent:<id>."
                )),
            },
        }
    }

    pub fn name(&self) -> String {
        match self {
            Target::Settings => "meerora.toml".into(),
            Target::Commands => "commands.toml".into(),
            Target::Agent(id) => format!("agent:{id}"),
            Target::NewAgent(id) => format!("new_agent:{id}"),
        }
    }

    /// The line under the helper's name while the proposal waits.
    fn activity(&self) -> String {
        match self {
            Target::Settings | Target::Commands => format!("proposes a change of {}", self.name()),
            Target::Agent(id) => format!("proposes a change of {id}"),
            Target::NewAgent(id) => format!("proposes a new agent, {id}"),
        }
    }
}

/// Lowercase letters, digits, `-` and `_`, starting with a letter, at most 40.
pub fn agent_id_ok(id: &str) -> bool {
    id.len() <= 40
        && id.starts_with(|c: char| c.is_ascii_lowercase())
        && id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

/// The files a proposal writes, in this order, and the directory of a new agent.
#[derive(Clone, Debug, PartialEq)]
struct Plan {
    writes: Vec<FileWrite>,
    new_dir: Option<PathBuf>,
}

#[derive(Clone, Debug, PartialEq)]
struct FileWrite {
    path: PathBuf,
    /// Relative to the data dir, for the diff and the messages.
    rel: String,
    /// The file as it was when the proposal was made, `None` when it did not exist.
    before: Option<String>,
    after: String,
}

impl Plan {
    fn diff(&self) -> String {
        self.writes.iter().map(|w| unified(&w.rel, w.before.as_deref(), &w.after)).collect::<Vec<_>>().join("")
    }

    fn changes_nothing(&self) -> bool {
        self.writes.iter().all(|w| w.before.as_deref() == Some(w.after.as_str()))
    }
}

/// One line of `audit.jsonl` about a configuration change.
#[derive(Serialize)]
struct AuditEntry<'a> {
    time: String,
    agent: &'a str,
    /// The target, for example `meerora.toml` or `agent:claude`.
    config: &'a str,
    /// propose, decision or close
    event: &'a str,
    /// apply or discard, for decisions.
    #[serde(skip_serializing_if = "Option::is_none")]
    decision: Option<&'a str>,
    /// user, meerora, or the agent for its own proposal.
    by: &'a str,
    request: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<&'a str>,
}

impl Daemon {
    /// `config_read` and `config_change` from the bridge `client`.
    pub(crate) fn config_tool(&mut self, client: u64, call: String, agent: &str, tool: ToolCall) {
        let answer = |daemon: &mut Daemon, ok: bool, message: String| {
            daemon.out.push(Outgoing::To(client, Event::ToolResult { id: call.clone(), ok, message, image: None }));
        };
        if !self.agents.get(agent).is_some_and(|a| is_helper(&a.dir.config)) {
            info!("{agent} called a configuration tool, but it is not the helper");
            return answer(self, false, "Only meerora's helper reads and changes the configuration.".into());
        }
        match tool {
            ToolCall::ConfigRead { target } if target.trim() == "check" => self.spawn_check(client, call),
            ToolCall::ConfigRead { target } => {
                let (ok, message) = match self.read_config(target.trim()) {
                    Ok(text) => (true, redact(&self.opts.data_dir, text)),
                    Err(e) => (false, e),
                };
                answer(self, ok, message);
            }
            ToolCall::ConfigChange { target, content, reason, figure } => {
                self.propose(client, call, agent, &target, content, reason, figure);
            }
            other => answer(self, false, format!("{other:?} is not a configuration tool")),
        }
    }

    /// The text of a `config_read` target other than `check`.
    pub(crate) fn read_config(&self, target: &str) -> Result<String, String> {
        let data = &self.opts.data_dir;
        if let Some(query) = target.strip_prefix("readme:") {
            return helper::manual(Some(query));
        }
        if let Some(id) = target.strip_prefix("agent:") {
            let id = id.trim();
            let Some(agent) = self.agents.get(id) else {
                let ids: Vec<&str> = self.agents.keys().map(String::as_str).collect();
                return Err(format!("There is no agent {id:?}. The agents are {}.", ids.join(", ")));
            };
            let path = agent.dir.path.join("agent.toml");
            return std::fs::read_to_string(&path).map_err(|e| format!("{} cannot be read ({e})", rel(data, &path)));
        }
        match target {
            "meerora.toml" => match std::fs::read_to_string(data.join("meerora.toml")) {
                Ok(text) => Ok(text),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(format!(
                    "There is no meerora.toml yet, so every setting has its default. A new one with the defaults as comments \
                     looks like this:\n\n{}",
                    crate::init::SETTINGS
                )),
                Err(e) => Err(format!("meerora.toml cannot be read ({e})")),
            },
            "agents" => Ok(self.agents_text()),
            "rooms" => Ok(self.rooms_text()),
            "commands" | "commands.toml" => Ok(commands_text(data)),
            "audit" => Ok(audit_tail(data, AUDIT_LINES)),
            "readme" => helper::manual(None),
            other => Err(format!("{other:?} is not a target. config_read takes {READ_TARGETS}.")),
        }
    }

    fn agents_text(&self) -> String {
        let mut text = String::from("The agents of the space, one directory each below agents/ of the data dir:\n");
        for (id, agent) in &self.agents {
            let c = &agent.dir.config;
            let mut parts = vec![c.name.clone()];
            match (&c.external, &c.runtime) {
                (Some(external), _) => {
                    let verified = if external.verified { "verified" } else { "not verified" };
                    parts.push(format!("external agent from {} ({verified})", external.domain));
                }
                (None, Some(runtime)) => {
                    let model = runtime.model.as_deref().map(|m| format!(" with {m}")).unwrap_or_default();
                    parts.push(format!("{}{model}", crate::scenario::kind_name(runtime.kind)));
                }
                (None, None) => parts.push("mock".into()),
            }
            if let Some(role) = c.role {
                parts.push(format!("role {}", role_name(role)));
            }
            if c.capabilities.system {
                parts.push("system agent".into());
            }
            parts.push(format!("home {}", c.home));
            let granted = self.granted(id);
            parts.push(if granted.is_empty() { "granted nothing".into() } else { format!("granted {}", granted.join(", ")) });
            if !c.workspace.is_empty() {
                let folders: Vec<String> = c.workspace.iter().map(|w| w.display().to_string()).collect();
                parts.push(format!("workspace {}", folders.join(", ")));
            }
            for entry in &c.schedule {
                let when = entry.every.as_deref().map(|e| format!("every {e}")).or_else(|| entry.at.as_deref().map(|a| format!("at {a}")));
                parts.push(format!("wakes {}", when.unwrap_or_default()));
            }
            text.push_str(&format!("- {id}: {}\n", parts.join(", ")));
        }
        let retired = load_graveyard(&self.opts.data_dir);
        if !retired.is_empty() {
            let names: Vec<String> = retired.iter().map(|r| format!("{} ({})", r.id, r.name)).collect();
            text.push_str(&format!("Retired agents in graveyard/: {}.\n", names.join(", ")));
        }
        text
    }

    fn rooms_text(&self) -> String {
        let data = &self.opts.data_dir;
        let rooms = meerora_protocol::rooms(data);
        let mut text = String::new();
        if let Some(current) = rooms.iter().find(|r| r.current) {
            text.push_str(&format!("Current room: {} ({}), files in {}/.\n", current.id, current.name, rel(data, &current.path)));
        }
        let others: Vec<String> = rooms
            .iter()
            .filter(|r| !r.current)
            .map(|r| if r.present { format!("{} ({})", r.id, r.name) } else { format!("{} ({}, not copied yet)", r.id, r.name) })
            .collect();
        if !others.is_empty() {
            text.push_str(&format!("Other rooms: {}. [space] room in meerora.toml picks one.\n", others.join(", ")));
        }
        text.push_str(&format!("Stations: {}.\n", self.opts.stations.join(", ")));
        let devices = self.device_infos();
        if devices.is_empty() {
            text.push_str("The room has no devices.\n");
            return text;
        }
        text.push_str("Devices:\n");
        for d in devices {
            let mut line = format!("- {}: {}", d.id, d.name);
            if let Some(owner) = &d.owner {
                line.push_str(&format!(", made by {owner}"));
            }
            let actions: Vec<String> = d
                .actions
                .iter()
                .map(|a| match &a.capability {
                    Some(capability) => format!("{} (needs {capability})", a.label),
                    None => a.label.clone(),
                })
                .collect();
            if !actions.is_empty() {
                line.push_str(&format!(". Actions: {}", actions.join(", ")));
            }
            if !d.granted.is_empty() {
                line.push_str(&format!(". Holds {}", d.granted.join(", ")));
            }
            if let Some(error) = &d.error {
                line.push_str(&format!(". Did not load: {}", first_line(error)));
            }
            text.push_str(&line);
            text.push_str(".\n");
        }
        text
    }

    /// Runs `meerora-agentd check` on a thread, because it builds every
    /// device and tries the sandbox, and answers when it is done.
    fn spawn_check(&mut self, client: u64, call: String) {
        let (data_dir, tx) = (self.opts.data_dir.clone(), self.tx.clone());
        let spawned = std::thread::Builder::new().name("config-check".into()).spawn(move || {
            let text = redact(&data_dir, crate::check_text(&data_dir));
            let _ = tx.send(Msg::Reply { client, event: Event::ToolResult { id: call, ok: true, message: text, image: None } });
        });
        if let Err(e) = spawned {
            warn!("cannot start the check ({e})");
        }
    }

    /// `config_change`: checks the proposal and shows it to the user.
    #[allow(clippy::too_many_arguments)]
    fn propose(&mut self, client: u64, call: String, agent: &str, target: &str, content: String, reason: String, figure: Option<String>) {
        let refuse = |daemon: &mut Daemon, message: String| {
            info!("the configuration change of {agent} was refused ({message})");
            daemon.out.push(Outgoing::To(client, Event::ToolResult { id: call.clone(), ok: false, message, image: None }));
        };
        let reason = cut(reason.trim(), REASON_MAX);
        if reason.is_empty() {
            return refuse(self, "Say in the reason why the user wants this change.".into());
        }
        let target = match Target::parse(target) {
            Ok(target) => target,
            Err(e) => return refuse(self, e),
        };
        let plan = match self.plan_change(&target, &content, figure.as_deref()) {
            Ok(plan) => plan,
            Err(e) => return refuse(self, e),
        };
        if plan.changes_nothing() {
            return refuse(self, format!("This is {} as it is, so there is nothing to apply.", target.name()));
        }
        let id = self.next_id("config");
        let diff = plan.diff();
        let name = target.name();
        info!("{agent} proposes a change of {name} ({id})");
        self.log_config(AuditEntry { note: Some(&reason), ..AuditEntry::new(agent, &name, "propose", agent, &id) });
        let wait = self.proposals.wait.unwrap_or(PROPOSAL_WAIT);
        let proposal =
            Proposal { id, agent: agent.into(), target, reason, diff, plan, waiters: vec![(client, call)], until: Instant::now() + wait };
        self.out.push(Outgoing::All(proposal_event(&proposal)));
        let activity = proposal.target.activity();
        self.proposals.open.push(proposal);
        // The figure waits for the user with a full ring (D3).
        self.emit(Event::Ring { agent: agent.into(), state: RingState::NeedsYou, progress: None });
        self.emit(Event::Activity { agent: agent.into(), text: cut(&activity, 60) });
    }

    /// Checks a proposal and works out what it writes.
    fn plan_change(&self, target: &Target, content: &str, figure: Option<&str>) -> Result<Plan, String> {
        let data = self.opts.data_dir.clone();
        if content.len() > CONTENT_MAX {
            return Err(format!("The content has {} bytes, at most {CONTENT_MAX} are allowed.", content.len()));
        }
        if figure.is_some() && !matches!(target, Target::NewAgent(_)) {
            return Err("figure is only for new_agent:<id>. Change the look of an agent with Edit agent in the builder.".into());
        }
        let ids: Vec<String> = self.agents.keys().cloned().collect();
        match target {
            Target::Settings => {
                check_settings(content, &ids, &data)?;
                let workspaces: Vec<PathBuf> = self.agents.values().flat_map(|a| a.dir.config.workspace.clone()).collect();
                check_binaries(content, &data, &workspaces)?;
                Ok(Plan { writes: vec![file_write(&data, "meerora.toml", content)], new_dir: None })
            }
            Target::Commands => {
                check_commands(content)?;
                Ok(Plan { writes: vec![file_write(&data, "commands.toml", content)], new_dir: None })
            }
            Target::Agent(id) => {
                let agent = self.agents.get(id).ok_or_else(|| {
                    format!("There is no agent {id:?}. To add one, use new_agent:{id}. The agents are {}.", ids.join(", "))
                })?;
                if agent.dir.config.external.is_some() {
                    return Err(format!(
                        "{id} comes from the meerora index, so its agent.toml follows its manifest. Import it again to change it."
                    ));
                }
                let config = parse_agent(content)?;
                self.check_agent(&config, Some(id))?;
                let rel = format!("agents/{id}/agent.toml");
                Ok(Plan { writes: vec![file_write(&data, &rel, content)], new_dir: None })
            }
            Target::NewAgent(id) => {
                let dir = data.join("agents").join(id);
                if std::fs::symlink_metadata(&dir).is_ok() {
                    return Err(format!("agents/{id} exists already. Pick another id, or change it with agent:{id}."));
                }
                let config = parse_agent(content)?;
                self.check_agent(&config, None)?;
                let script = figure.map(str::to_string).unwrap_or_else(|| helper::NEW_AGENT_FIGURE.to_string());
                if script.len() > CONTENT_MAX {
                    return Err(format!("The figure has {} bytes, at most {CONTENT_MAX} are allowed.", script.len()));
                }
                meerora_script::build_figure(&script, "figure.js", &meerora_script::Limits::default())
                    .map_err(|e| format!("The figure script does not pass the validator: {e}"))?;
                let mut script = script;
                if !script.ends_with('\n') {
                    script.push('\n');
                }
                let figure = FileWrite { path: dir.join("figure.js"), rel: format!("agents/{id}/figure.js"), before: None, after: script };
                // agent.toml last, so the agent never appears without its figure.
                let toml = file_write(&data, &format!("agents/{id}/agent.toml"), content);
                Ok(Plan { writes: vec![figure, toml], new_dir: Some(dir) })
            }
        }
    }

    /// The checks of an agent.toml beyond parsing. `id` is the agent it
    /// replaces, `None` for a new agent.
    fn check_agent(&self, config: &AgentConfig, id: Option<&str>) -> Result<(), String> {
        let old = id.and_then(|id| self.agents.get(id)).map(|a| &a.dir.config);
        let name = config.name.trim();
        if name.is_empty() || name.chars().count() > 40 {
            return Err("name needs 1 to 40 characters.".into());
        }
        if !color_ok(&config.color) {
            return Err(format!("color is a colour like \"#8CD3FF\", not {:?}.", config.color));
        }
        let figure_wanted = old.map_or("figure.js", |o| o.figure.as_str());
        if config.figure != figure_wanted {
            return Err(format!("figure stays {figure_wanted:?}. Change the look with Edit agent in the builder."));
        }
        let stations = &self.opts.stations;
        if !stations.contains(&config.home) {
            return Err(format!("home is a station of the space, not {:?}. The stations are {}.", config.home, stations.join(", ")));
        }
        for (i, entry) in config.schedule.iter().enumerate() {
            crate::schedule::check_entry(entry).map_err(|e| format!("Schedule entry {}: {e}.", i + 1))?;
            if let Some(station) = entry.station.as_ref().filter(|s| !stations.contains(s)) {
                return Err(format!("Schedule entry {} names the station {station:?}, which the space does not have.", i + 1));
            }
        }
        let old_kind = old.and_then(|o| o.runtime.as_ref()).map(|r| r.kind);
        match config.runtime.as_ref().map(|r| r.kind) {
            Some(RuntimeKind::ClaudeCli | RuntimeKind::CodexCli | RuntimeKind::Ollama) => {}
            Some(RuntimeKind::Mock) | None if old_kind == Some(RuntimeKind::Mock) || (old.is_some() && old_kind.is_none()) => {}
            Some(RuntimeKind::Api) => return Err("The api runtime does not exist yet. Use claude-cli, codex-cli or ollama.".into()),
            Some(RuntimeKind::External) => return Err("Only agents imported from the index run on external.".into()),
            Some(RuntimeKind::Mock) | None => {
                return Err("[runtime] kind is claude-cli, codex-cli or ollama, so the agent can do something.".into());
            }
        }
        if config.external != old.and_then(|o| o.external.clone()) {
            return Err("[external] belongs to agents imported from the index and cannot be changed here.".into());
        }
        let helper = old.is_some_and(is_helper);
        match config.role {
            Some(AgentRole::Helper) if !helper => {
                return Err("Only one agent is meerora's helper, and that is you. Leave role out or pick another role.".into());
            }
            role if helper && role != Some(AgentRole::Helper) => {
                return Err("You are meerora's helper, so your agent.toml keeps role = \"helper\".".into());
            }
            Some(role @ (AgentRole::Personal | AgentRole::News)) => {
                if let Some((other, _)) =
                    self.agents.iter().find(|(other, a)| Some(other.as_str()) != id && a.dir.config.role == Some(role))
                {
                    return Err(format!("{other} has role = \"{}\" already, and there is only one.", role_name(role)));
                }
            }
            _ => {}
        }
        let mut seen = Vec::new();
        for capability in &config.capabilities.granted {
            crate::capabilities::check_name(capability)?;
            if seen.contains(&capability) {
                return Err(format!("{capability} is granted twice."));
            }
            seen.push(capability);
        }
        let home = meerora_runtime::sandbox::user_home();
        for folder in &config.workspace {
            meerora_protocol::workspace::check(folder, home.as_deref(), &self.opts.data_dir)
                .map_err(|e| format!("The workspace folder {} is refused: {e}", folder.display()))?;
        }
        Ok(())
    }

    /// `Command::DecideConfig` from the user, from `client`.
    pub(crate) fn decide_config(&mut self, client: u64, id: &str, apply: bool) {
        let Some(index) = self.proposals.open.iter().position(|p| p.id == id) else {
            info!("configuration proposal {id} is unknown or already decided");
            let message = Some("This proposal is unknown or was decided already.".to_string());
            self.out.push(Outgoing::To(client, Event::ConfigResult { id: id.into(), applied: false, message }));
            return;
        };
        let proposal = self.proposals.open.remove(index);
        let (agent, name) = (proposal.agent.clone(), proposal.target.name());
        let helper = self.agents.get(&agent).map_or(agent.clone(), |a| a.dir.config.name.clone());
        let (applied, for_user, for_agent) = if apply {
            match self.write_plan(&proposal.plan, &helper) {
                Ok(()) => {
                    self.reload_after(&proposal.target);
                    let done = match &proposal.target {
                        Target::NewAgent(new) => format!("The agent {new} is in the space now."),
                        Target::Commands => "The command box reads it again by itself.".into(),
                        _ => "meerora uses it now.".into(),
                    };
                    (true, format!("{name} is written. {done}"), format!("The user applied the change. {name} is written. {done}"))
                }
                Err(e) => {
                    warn!("the change of {name} by {agent} was applied but not written ({e})");
                    (false, e.clone(), format!("The user applied the change, but it was not written: {e}"))
                }
            }
        } else {
            let text = "Nothing was written.".to_string();
            (false, text, "The user discarded the change, so nothing was written. Ask what they want instead, if that is unclear.".into())
        };
        info!("the user decided {} for the change of {name} by {agent} ({id})", if apply { "apply" } else { "discard" });
        self.log_config(AuditEntry {
            decision: Some(if apply { "apply" } else { "discard" }),
            note: Some(&for_user),
            ..AuditEntry::new(&agent, &name, "decision", "user", id)
        });
        for (client, call) in &proposal.waiters {
            let result = Event::ToolResult { id: call.clone(), ok: true, message: for_agent.clone(), image: None };
            self.out.push(Outgoing::To(*client, result));
        }
        self.out.push(Outgoing::All(Event::ConfigResult { id: id.into(), applied, message: Some(for_user) }));
        let activity = if applied { format!("{name} changed") } else { format!("{name} stays as it was") };
        self.proposal_done(&agent, activity);
    }

    /// Writes the files of a plan, unless one changed since the proposal.
    fn write_plan(&self, plan: &Plan, helper: &str) -> Result<(), String> {
        for w in &plan.writes {
            let now = read_optional(&w.path).map_err(|e| format!("{} cannot be read ({e}), so nothing was written.", w.rel))?;
            if now != w.before {
                return Err(format!("{} changed since {helper} proposed this, so nothing was written. Ask for a new proposal.", w.rel));
            }
        }
        if let Some(dir) = &plan.new_dir {
            std::fs::create_dir(dir)
                .map_err(|e| format!("{} cannot be created ({e}), so nothing was written.", rel(&self.opts.data_dir, dir)))?;
        }
        for w in &plan.writes {
            write_atomic(&w.path, &w.after).map_err(|e| format!("{} cannot be written ({e}).", w.rel))?;
        }
        Ok(())
    }

    /// Makes meerora use what was written.
    fn reload_after(&mut self, target: &Target) {
        match target {
            Target::Settings => {
                // A room the data dir lacks is copied from its template, as the room picker does.
                if let Err(e) = meerora_protocol::ensure_chosen_room(&self.opts.data_dir) {
                    warn!("the room of meerora.toml is not ready ({e})");
                }
                self.handle_command(0, meerora_protocol::Command::ReloadAgents);
            }
            Target::Agent(_) | Target::NewAgent(_) => self.handle_command(0, meerora_protocol::Command::ReloadAgents),
            Target::Commands => {}
        }
    }

    /// The ring of a running helper goes back to working once nothing of it waits any more.
    fn proposal_done(&mut self, agent: &str, activity: String) {
        let running = self.agents.get(agent).and_then(|a| a.live.as_ref()).is_some_and(|l| l.run.is_some());
        if running && !self.proposals.open.iter().any(|p| p.agent == agent) {
            self.emit(Event::Ring { agent: agent.into(), state: RingState::Working, progress: None });
            self.emit(Event::Activity { agent: agent.into(), text: cut(&activity, 60) });
        }
    }

    /// Closes the open proposals of `agent` without writing anything, for
    /// example because its run was cancelled.
    pub(crate) fn close_proposals(&mut self, agent: &str, why: &str) {
        let (closed, open): (Vec<Proposal>, Vec<Proposal>) =
            std::mem::take(&mut self.proposals.open).into_iter().partition(|p| p.agent == agent);
        self.proposals.open = open;
        for proposal in closed {
            self.close(proposal, why);
        }
    }

    fn close(&mut self, proposal: Proposal, why: &str) {
        let name = proposal.target.name();
        info!("the change of {name} by {} is closed, because {why}", proposal.agent);
        self.log_config(AuditEntry { note: Some(why), ..AuditEntry::new(&proposal.agent, &name, "close", "meerora", &proposal.id) });
        let message = format!("The proposal for {name} was closed, because {why}. Nothing was written.");
        for (client, call) in &proposal.waiters {
            let result = Event::ToolResult { id: call.clone(), ok: true, message: message.clone(), image: None };
            self.out.push(Outgoing::To(*client, result));
        }
        self.out.push(Outgoing::All(Event::ConfigResult { id: proposal.id, applied: false, message: Some(message) }));
    }

    /// Closes the proposals nobody decided in time.
    pub(crate) fn tick_proposals(&mut self, now: Instant) {
        let (late, open): (Vec<Proposal>, Vec<Proposal>) =
            std::mem::take(&mut self.proposals.open).into_iter().partition(|p| p.until <= now);
        self.proposals.open = open;
        for proposal in late {
            let agent = proposal.agent.clone();
            self.close(proposal, "nobody decided it in time");
            self.proposal_done(&agent, "the proposal timed out".into());
        }
    }

    /// How long until the next proposal times out.
    pub(crate) fn proposals_wait(&self, now: Instant) -> Option<Duration> {
        self.proposals.open.iter().map(|p| p.until.saturating_duration_since(now)).min()
    }

    /// The open proposals, for the greeting.
    pub(crate) fn config_greeting(&self) -> Vec<Event> {
        self.proposals.open.iter().map(proposal_event).collect()
    }

    /// Writes one line to the audit log. A failure is logged, it never stops a decision.
    fn log_config(&self, entry: AuditEntry) {
        let entry = AuditEntry { time: jiff::Zoned::now().strftime("%Y-%m-%dT%H:%M:%S%:z").to_string(), ..entry };
        let path = self.opts.data_dir.join("audit.jsonl");
        let written = serde_json::to_string(&entry)
            .map_err(std::io::Error::other)
            .and_then(|line| std::fs::OpenOptions::new().create(true).append(true).open(&path)?.write_all(format!("{line}\n").as_bytes()));
        if let Err(e) = written {
            warn!("cannot write the audit log {} ({e})", path.display());
        }
    }
}

impl<'a> AuditEntry<'a> {
    fn new(agent: &'a str, config: &'a str, event: &'a str, by: &'a str, request: &'a str) -> Self {
        Self { time: String::new(), agent, config, event, decision: None, by, request, note: None }
    }
}

fn proposal_event(p: &Proposal) -> Event {
    let path = p.plan.writes.last().map(|w| w.rel.clone()).unwrap_or_default();
    Event::ConfigProposal {
        id: p.id.clone(),
        agent: p.agent.clone(),
        target: p.target.name(),
        path,
        reason: p.reason.clone(),
        diff: p.diff.clone(),
    }
}

fn role_name(role: AgentRole) -> &'static str {
    match role {
        AgentRole::Personal => "personal",
        AgentRole::News => "news",
        AgentRole::Helper => "helper",
    }
}

/// A file of the data dir as it is and as the proposal would write it. The
/// leading comment lines of the old file stay when the new content has none.
fn file_write(data: &Path, rel: &str, content: &str) -> FileWrite {
    let path = data.join(rel);
    let before = read_optional(&path).ok().flatten();
    let after = keep_header(before.as_deref().unwrap_or(""), content);
    FileWrite { path, rel: rel.into(), before, after }
}

/// `new` with the leading comment lines of `old` when it has none of its
/// own, ending with a newline.
pub fn keep_header(old: &str, new: &str) -> String {
    let mut text = if new.lines().next().is_some_and(|l| l.starts_with('#')) {
        new.to_string()
    } else {
        let header: String = old.lines().take_while(|l| l.starts_with('#')).map(|l| format!("{l}\n")).collect();
        format!("{header}{new}")
    };
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text
}

fn read_optional(path: &Path) -> std::io::Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Through a temporary file, and never through a symlink an agent put into
/// its directory (safefs).
fn write_atomic(path: &Path, text: &str) -> std::io::Result<()> {
    meerora_protocol::safefs::replace_file(path, text.as_bytes())
}

fn rel(data: &Path, path: &Path) -> String {
    path.strip_prefix(data).unwrap_or(path).display().to_string()
}

fn first_line(text: &str) -> &str {
    text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("")
}

/// A unified diff of one file, with the TOML table a hunk starts in after
/// its `@@` head.
pub fn unified(rel: &str, before: Option<&str>, after: &str) -> String {
    let old = before.unwrap_or("");
    let diff = TextDiff::configure().algorithm(Algorithm::Myers).diff_lines(old, after);
    let groups = diff.grouped_ops(3);
    if groups.is_empty() {
        return String::new();
    }
    let toml = rel.ends_with(".toml");
    let old_lines: Vec<&str> = old.lines().collect();
    let mut out = match before {
        Some(_) => format!("--- a/{rel}\n+++ b/{rel}\n"),
        None => format!("--- /dev/null\n+++ b/{rel}\n"),
    };
    for group in groups {
        let (Some(first), Some(last)) = (group.first(), group.last()) else { continue };
        let (old_start, old_end) = (first.old_range().start, last.old_range().end);
        let (new_start, new_end) = (first.new_range().start, last.new_range().end);
        let range = |start: usize, end: usize| {
            let len = end - start;
            format!("{},{len}", if len == 0 { start } else { start + 1 })
        };
        let section = if toml {
            old_lines[..old_start.min(old_lines.len())]
                .iter()
                .rev()
                .map(|l| l.trim())
                .find(|l| l.starts_with('['))
                .map(|l| format!(" {l}"))
                .unwrap_or_default()
        } else {
            String::new()
        };
        out.push_str(&format!("@@ -{} +{} @@{section}\n", range(old_start, old_end), range(new_start, new_end)));
        for op in &group {
            for change in diff.iter_changes(op) {
                let sign = match change.tag() {
                    ChangeTag::Equal => ' ',
                    ChangeTag::Delete => '-',
                    ChangeTag::Insert => '+',
                };
                out.push(sign);
                out.push_str(change.value().trim_end_matches(['\n', '\r']));
                out.push('\n');
            }
        }
    }
    out
}

/// The index token never leaves the daemon, even when a text holds it.
fn redact(data: &Path, text: String) -> String {
    let token = std::fs::read_to_string(data.join(crate::social::ACCOUNT_FILE))
        .ok()
        .and_then(|t| serde_json::from_str::<crate::social::AccountFile>(&t).ok())
        .map(|a| a.token)
        .filter(|t| t.len() >= 8);
    match token {
        Some(token) if text.contains(&token) => text.replace(&token, "[hidden]"),
        _ => text,
    }
}

fn commands_text(data: &Path) -> String {
    let mut text = match std::fs::read_to_string(data.join("commands.toml")) {
        Ok(text) => text,
        Err(_) => "There is no commands.toml yet. The command box writes a commented example the first time it opens, and you can \
                   propose one with config_change."
            .to_string(),
    };
    let mut scripts: Vec<String> = std::fs::read_dir(data.join("commands"))
        .map(|entries| {
            entries.filter_map(|e| e.ok()).filter_map(|e| e.file_name().to_str().map(String::from)).filter(|n| n.ends_with(".js")).collect()
        })
        .unwrap_or_default();
    scripts.sort();
    if !scripts.is_empty() {
        text.push_str(&format!("\n\nCommand scripts in commands/: {}.\n", scripts.join(", ")));
    }
    text
}

/// The last `n` lines of `audit.jsonl`.
fn audit_tail(data: &Path, n: usize) -> String {
    let text = std::fs::read_to_string(data.join("audit.jsonl")).unwrap_or_default();
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.is_empty() {
        return "The audit log is empty.".into();
    }
    let start = lines.len().saturating_sub(n);
    format!("The last {} lines of audit.jsonl, oldest first:\n{}\n", lines.len() - start, lines[start..].join("\n"))
}

fn color_ok(color: &str) -> bool {
    color.len() == 7 && color.starts_with('#') && color[1..].chars().all(|c| c.is_ascii_hexdigit())
}

fn parse_agent(content: &str) -> Result<AgentConfig, String> {
    toml::from_str::<AgentConfig>(content).map_err(|e| format!("The agent.toml does not parse: {}", e.to_string().trim()))
}

/// `[commands]` of `meerora.toml`, as the app reads it (ui/commands/config.rs).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CommandSettings {
    #[serde(default)]
    key: Option<String>,
    #[serde(default)]
    assistant: Option<String>,
    #[serde(default)]
    search: Option<String>,
}

/// Checks a proposed `meerora.toml` the way the daemon and the app read it.
pub fn check_settings(content: &str, agents: &[String], data_dir: &Path) -> Result<(), String> {
    let settings: Settings = toml::from_str(content).map_err(|e| format!("meerora.toml does not parse: {}", e.to_string().trim()))?;
    if let Some(table) = settings.commands {
        let commands: CommandSettings =
            toml::Value::Table(table).try_into().map_err(|e: toml::de::Error| format!("[commands]: {}", e.to_string().trim()))?;
        if let Some(key) = &commands.key {
            hotkey_ok(key).map_err(|e| format!("[commands] key: {e}"))?;
        }
        if let Some(assistant) = commands.assistant.as_deref().map(str::trim)
            && !agents.iter().any(|a| a == assistant)
        {
            return Err(format!(
                "[commands] assistant names an agent, and there is no {assistant:?}. The agents are {}.",
                agents.join(", ")
            ));
        }
        if let Some(search) = &commands.search
            && !(search.starts_with("https://") || search.starts_with("http://"))
        {
            return Err(format!("[commands] search is an http or https URL with {{query}}, not {search:?}."));
        }
    }
    if let Some(room) = settings.space.room.as_deref() {
        let known = meerora_protocol::TEMPLATES.iter().any(|t| t.id == room)
            || meerora_protocol::room_dir(data_dir, room).join("space.js").is_file();
        if !meerora_protocol::room_name_ok(room) || !known {
            let ids: Vec<&str> = meerora_protocol::TEMPLATES.iter().map(|t| t.id).collect();
            return Err(format!("[space] room is one of {} or a folder in spaces/, not {room:?}.", ids.join(", ")));
        }
    }
    if let Some(url) = &settings.index.url
        && !(url.starts_with("https://") || url.starts_with("http://"))
    {
        return Err(format!("[index] url is an http or https URL, not {url:?}."));
    }
    if let Some(builder) = &settings.builder
        && !matches!(builder.kind, RuntimeKind::ClaudeCli | RuntimeKind::CodexCli | RuntimeKind::Ollama)
    {
        return Err("[builder] kind is claude-cli, codex-cli or ollama.".into());
    }
    Ok(())
}

/// `[paths] claude` and `codex` name programs that meerora starts, the
/// builder outside the sandbox. Agents write their directories and their
/// workspace folders, so a program there would run what an agent wrote.
pub fn check_binaries(content: &str, data_dir: &Path, workspaces: &[PathBuf]) -> Result<(), String> {
    let settings: Settings = toml::from_str(content).map_err(|e| format!("meerora.toml does not parse: {}", e.to_string().trim()))?;
    let real = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    let inside = |path: &Path, folder: &Path| path.starts_with(folder) || real(path).starts_with(real(folder));
    for (key, path) in [("claude", &settings.paths.claude), ("codex", &settings.paths.codex)] {
        let Some(path) = path else { continue };
        if !path.is_absolute() {
            return Err(format!("[paths] {key} is an absolute path, not {}.", path.display()));
        }
        if inside(path, data_dir) || workspaces.iter().any(|w| inside(path, w)) {
            return Err(format!(
                "[paths] {key} must not point into meerora's data dir or a workspace folder, because agents write there. {} is refused.",
                path.display()
            ));
        }
    }
    Ok(())
}

/// A `[[command]]` of `commands.toml`, as the app reads it.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CommandEntry {
    keyword: String,
    title: String,
    #[serde(default)]
    subtitle: Option<String>,
    #[serde(default)]
    open_url: Option<String>,
    #[serde(default)]
    launch: Option<Vec<String>>,
    #[serde(default)]
    say: Option<String>,
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    open_app: Option<String>,
    #[serde(default)]
    copy: Option<String>,
    #[serde(default)]
    notify: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CommandFile {
    #[serde(default)]
    command: Vec<CommandEntry>,
}

/// Checks a proposed `commands.toml`.
pub fn check_commands(content: &str) -> Result<(), String> {
    let file: CommandFile = toml::from_str(content).map_err(|e| format!("commands.toml does not parse: {}", e.to_string().trim()))?;
    let mut keywords: Vec<&str> = Vec::new();
    for (i, c) in file.command.iter().enumerate() {
        let n = i + 1;
        let keyword = c.keyword.trim();
        if keyword.is_empty() || keyword.contains(char::is_whitespace) {
            return Err(format!("Command {n} needs a keyword of one word."));
        }
        if keywords.contains(&keyword) {
            return Err(format!("The keyword {keyword:?} is used twice."));
        }
        keywords.push(keyword);
        if c.title.trim().is_empty() {
            return Err(format!("Command {n} ({keyword}) needs a title."));
        }
        let actions =
            [c.open_url.is_some(), c.launch.is_some(), c.say.is_some(), c.open_app.is_some(), c.copy.is_some(), c.notify.is_some()];
        if actions.iter().filter(|a| **a).count() != 1 {
            return Err(format!("Command {n} ({keyword}) has exactly one of open_url, launch, say, open_app, copy or notify."));
        }
        if c.agent.is_some() && c.say.is_none() {
            return Err(format!("Command {n} ({keyword}) names an agent, which only say uses."));
        }
        if c.launch.as_ref().is_some_and(|l| l.is_empty() || l[0].trim().is_empty()) {
            return Err(format!("Command {n} ({keyword}) launches nothing. launch is a list like [\"gnome-calculator\"]."));
        }
        let _ = &c.subtitle;
    }
    Ok(())
}

/// Whether a key such as `Alt+Space`, `Ctrl+Space` or `F12` can open the
/// command box, as the app's `Hotkey::parse` reads it.
pub fn hotkey_ok(text: &str) -> Result<(), String> {
    let parts: Vec<&str> = text.split('+').map(str::trim).collect();
    let Some((name, mods)) = parts.split_last().filter(|(n, _)| !n.is_empty()) else {
        return Err(format!("{text:?} names no key"));
    };
    let mut modifier = false;
    for m in mods {
        match m.to_ascii_lowercase().as_str() {
            "meta" | "super" | "logo" | "win" | "cmd" | "ctrl" | "control" | "alt" => modifier = true,
            "shift" => {}
            other => return Err(format!("{other:?} in {text:?} is not Meta, Ctrl, Alt or Shift")),
        }
    }
    let lower = name.to_ascii_lowercase();
    let function = lower.strip_prefix('f').and_then(|n| n.parse::<u8>().ok()).is_some_and(|n| (1..=12).contains(&n));
    let single = lower.chars().count() == 1 && lower.chars().all(|c| c.is_ascii_alphanumeric());
    let named = [
        "space",
        "enter",
        "return",
        "tab",
        "slash",
        "period",
        "dot",
        "comma",
        "semicolon",
        "minus",
        "equal",
        "equals",
        "backquote",
        "grave",
        "backslash",
    ]
    .contains(&lower.as_str());
    if !(function || single || named) {
        return Err(format!("{name:?} in {text:?} is not a key name such as Space, K, F12 or Slash"));
    }
    if !modifier && !function {
        return Err(format!("{text:?} needs Meta, Ctrl or Alt, or must be a function key, so typing is not taken"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_parse_and_ids_are_checked() {
        assert_eq!(Target::parse(" meerora.toml "), Ok(Target::Settings));
        assert_eq!(Target::parse("commands"), Ok(Target::Commands));
        assert_eq!(Target::parse("agent:claude"), Ok(Target::Agent("claude".into())));
        assert_eq!(Target::parse("new_agent:mail-bot"), Ok(Target::NewAgent("mail-bot".into())));
        for bad in ["agent:../runtimes", "new_agent:Bob", "agent:", "runtimes/claude", "index-account.json", "audit"] {
            assert!(Target::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn headers_stay_and_diffs_name_the_table() {
        assert_eq!(keep_header("# one\n# two\nname = \"A\"\n", "name = \"B\""), "# one\n# two\nname = \"B\"\n");
        assert_eq!(keep_header("# one\nname = \"A\"\n", "# mine\nname = \"B\"\n"), "# mine\nname = \"B\"\n");
        let old = "name = \"Claude\"\nhome = \"bookshelf\"\n\n[runtime]\nkind = \"claude-cli\"\n\n[capabilities]\ngranted = []\n";
        let new = old.replace("granted = []", "granted = [\"web\"]");
        let diff = unified("agents/claude/agent.toml", Some(old), &new);
        assert!(diff.starts_with("--- a/agents/claude/agent.toml\n+++ b/agents/claude/agent.toml\n@@ -5,4 +5,4 @@ [runtime]\n"), "{diff}");
        assert!(diff.contains("\n-granted = []\n+granted = [\"web\"]\n"), "{diff}");
        let created = unified("agents/x/figure.js", None, "a\nb\n");
        assert_eq!(created, "--- /dev/null\n+++ b/agents/x/figure.js\n@@ -0,0 +1,2 @@\n+a\n+b\n");
        assert_eq!(unified("meerora.toml", Some("a\n"), "a\n"), "");
    }

    #[test]
    fn settings_and_commands_are_checked_like_meerora_reads_them() {
        let agents = vec!["claude".to_string(), "rivet".to_string()];
        let data = Path::new("/nowhere");
        assert!(check_settings("[commands]\nkey = \"Ctrl+Space\"\nassistant = \"rivet\"\n", &agents, data).is_ok());
        assert!(check_settings("# only comments\n", &agents, data).is_ok());
        let err = |text: &str| check_settings(text, &agents, data).unwrap_err();
        assert!(err("[comands]\nkey = \"x\"\n").contains("does not parse"));
        assert!(err("[commands]\nkey = \"Space\"\n").contains("needs Meta, Ctrl or Alt"));
        assert!(err("[commands]\nkey = \"Ctrl+Banana\"\n").contains("not a key name"));
        assert!(err("[commands]\nassistant = \"ember\"\n").contains("no \"ember\""));
        assert!(err("[commands]\ncolour = 1\n").contains("[commands]"));
        assert!(err("[space]\nroom = \"moon\"\n").contains("[space] room"));
        assert!(err("[index]\nurl = \"ftp://x\"\n").contains("[index] url"));
        assert!(err("[builder]\nkind = \"mock\"\n").contains("[builder]"));
        assert!(check_settings("[space]\nroom = \"office\"\n", &agents, data).is_ok());
        assert!(hotkey_ok("F12").is_ok() && hotkey_ok("Meta+Shift+K").is_ok() && hotkey_ok("Alt+").is_err());
        let work = PathBuf::from("/home/u/code/app");
        let binary = |path: &str| check_binaries(&format!("[paths]\nclaude = \"{path}\"\n"), data, std::slice::from_ref(&work));
        assert!(binary("/usr/bin/claude").is_ok());
        assert!(binary("/nowhere/agents/rivet/claude").unwrap_err().contains("must not point into"));
        assert!(binary("/home/u/code/app/tools/claude").unwrap_err().contains("must not point into"));
        assert!(binary("bin/claude").unwrap_err().contains("absolute"));

        assert!(check_commands("[[command]]\nkeyword = \"w\"\ntitle = \"Wiki\"\nopen_url = \"https://w.example/{query}\"\n").is_ok());
        assert!(check_commands("").is_ok());
        let two = "[[command]]\nkeyword = \"w\"\ntitle = \"W\"\ncopy = \"a\"\nnotify = \"b\"\n";
        assert!(check_commands(two).unwrap_err().contains("exactly one"));
        let twice =
            "[[command]]\nkeyword = \"w\"\ntitle = \"W\"\ncopy = \"a\"\n[[command]]\nkeyword = \"w\"\ntitle = \"V\"\ncopy = \"b\"\n";
        assert!(check_commands(twice).unwrap_err().contains("twice"));
        assert!(check_commands("[[command]]\nkeyword = \"a b\"\ntitle = \"W\"\ncopy = \"a\"\n").is_err());
    }

    #[test]
    fn the_protocol_messages_round_trip() {
        use meerora_protocol::{Command, decode_command, decode_event, encode};
        let call = ToolCall::ConfigChange { target: "meerora.toml".into(), content: "x".into(), reason: "y".into(), figure: None };
        let command = Command::Tool { id: "1".into(), agent: "rivet".into(), call };
        assert_eq!(decode_command(&encode(&command)).unwrap(), command);
        let decide = Command::DecideConfig { id: "config-1".into(), apply: true };
        assert_eq!(encode(&decide), "{\"type\":\"decide_config\",\"id\":\"config-1\",\"apply\":true}\n");
        let event = Event::ConfigResult { id: "config-1".into(), applied: true, message: None };
        assert_eq!(decode_event(&encode(&event)).unwrap(), event);
        let read = decode_command(r#"{"type":"tool","id":"2","agent":"rivet","call":{"tool":"config_read","target":"agents"}}"#).unwrap();
        assert!(matches!(read, Command::Tool { call: ToolCall::ConfigRead { .. }, .. }));
    }
}
