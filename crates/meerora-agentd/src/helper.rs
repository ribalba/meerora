//! Rivet, the helper: the permanent system agent that helps the user
//! configure meerora and the system (`role = "helper"` in `agent.toml`).
//!
//! The daemon makes sure a helper exists when it starts ([`ensure`]). When no
//! agent has the role, it writes `agents/rivet/` from the template in
//! `assets/templates/helper/`, which is built into the binary. A directory
//! `agents/rivet/` that is not the helper, such as the mock Rivet of the demo
//! data in `assets/`, is never overwritten. The daemon then runs without a
//! helper and says why in its log (the README tells how to migrate). The
//! helper cannot be retired (graveyard.rs), and only the helper gets the
//! tools `config_read` and `config_change` (config_tools.rs).
//!
//! The helper reads the README, meerora's manual, through `config_read
//! readme` ([`manual`]). A copy in the data dir would not do, because the
//! claude CLI's `--restricted` keeps its file tools inside the agent's own
//! directory.

use std::path::{Path, PathBuf};

use meerora_protocol::{AgentConfig, AgentRole, load_agents};
use tracing::warn;

/// The directory name of a helper that the daemon creates.
pub const ID: &str = "rivet";
/// The helper's `agent.toml`: `role = "helper"`, system privileges, claude-cli with sonnet.
pub const AGENT_TOML: &str = include_str!("../../../assets/templates/helper/agent.toml");
/// The robot figure.
pub const FIGURE: &str = include_str!("../../../assets/templates/helper/figure.js");
/// The figure of an agent that the helper creates without one (`new_agent:<id>`).
pub const NEW_AGENT_FIGURE: &str = include_str!("../../../assets/templates/helper/new-agent.js");
/// The README the helper reads, as it was when the daemon was built.
const README: &str = include_str!("../../../README.md");
/// Most characters [`manual`] returns for one question.
const MANUAL_MAX: usize = 16_000;
/// The home station of the template. Every shipped room has it.
const HOME: &str = "router_cabinet";

/// Whether an agent is the helper. An agent from the index never is, even
/// if its agent.toml said so, because the helper reads the configuration.
pub fn is_helper(config: &AgentConfig) -> bool {
    config.role == Some(AgentRole::Helper) && config.external.is_none()
}

/// What [`ensure`] found or did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ensured {
    /// An agent with the role exists, here its id.
    Present(String),
    /// The daemon wrote the helper into this directory.
    Created(PathBuf),
    /// There is no helper and none could be created, with the reason.
    Missing(String),
}

/// Makes sure an agent with `role = "helper"` exists below `data_dir`.
/// `stations` are the stations of the room, for its home.
pub fn ensure(data_dir: &Path, stations: &[String]) -> Ensured {
    let helpers: Vec<String> =
        load_agents(data_dir).into_iter().filter_map(Result::ok).filter(|a| is_helper(&a.config)).map(|a| a.id).collect();
    if let Some(first) = helpers.first() {
        if helpers.len() > 1 {
            warn!("{} all have role = \"helper\", but only one agent should be the helper", helpers.join(", "));
        }
        return Ensured::Present(first.clone());
    }
    let dir = data_dir.join("agents").join(ID);
    if std::fs::symlink_metadata(&dir).is_ok() {
        return Ensured::Missing(format!(
            "{} is not the helper (its agent.toml has no role = \"helper\" or does not parse), so meerora runs without its \
             helper. The README says how to move it aside so the daemon creates the helper",
            dir.display()
        ));
    }
    match create(data_dir, stations) {
        Ok(dir) => Ensured::Created(dir),
        Err(e) => Ensured::Missing(format!("the helper could not be created ({e})")),
    }
}

/// Writes `agents/rivet/` from the template. Refuses when the directory exists.
pub fn create(data_dir: &Path, stations: &[String]) -> Result<PathBuf, String> {
    let dir = data_dir.join("agents").join(ID);
    if std::fs::symlink_metadata(&dir).is_ok() {
        return Err(format!("{} exists already", dir.display()));
    }
    let write = || -> std::io::Result<()> {
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join("figure.js"), FIGURE)?;
        // agent.toml last, so the agent never appears without its figure.
        std::fs::write(dir.join("agent.toml"), config_text(stations))
    };
    write().map_err(|e| format!("cannot write {} ({e})", dir.display()))?;
    Ok(dir)
}

/// The template's `agent.toml`, with another home when the room has no router cabinet.
pub fn config_text(stations: &[String]) -> String {
    if stations.is_empty() || stations.iter().any(|s| s == HOME) {
        return AGENT_TOML.to_string();
    }
    let home = stations.iter().find(|s| *s != "door").unwrap_or(&stations[0]);
    AGENT_TOML.replacen(&format!("home = \"{HOME}\""), &format!("home = \"{home}\""), 1)
}

/// One `##` or `###` section of the README.
struct Section<'a> {
    heading: &'a str,
    text: String,
}

/// The sections of the README, each with its heading line. Headings inside
/// code blocks do not count.
fn sections() -> Vec<Section<'static>> {
    let mut out: Vec<Section> = Vec::new();
    let mut fence = false;
    for line in README.lines() {
        if line.trim_start().starts_with("```") {
            fence = !fence;
        }
        let heading = (!fence).then(|| line.strip_prefix("### ").or_else(|| line.strip_prefix("## "))).flatten();
        match (heading, out.last_mut()) {
            (Some(heading), _) => out.push(Section { heading: heading.trim(), text: format!("{line}\n") }),
            (None, Some(section)) => {
                section.text.push_str(line);
                section.text.push('\n');
            }
            (None, None) => {}
        }
    }
    out
}

/// The README for the helper. Without a query it lists the sections, with
/// one it returns the sections whose heading matches, else those whose text
/// holds every word.
pub fn manual(query: Option<&str>) -> Result<String, String> {
    let sections = sections();
    let headings = || sections.iter().map(|s| format!("- {}", s.heading)).collect::<Vec<_>>().join("\n");
    let Some(query) = query.map(str::trim).filter(|q| !q.is_empty()) else {
        return Ok(format!("meerora's README has these sections. config_read readme:<words> returns the ones that match.\n{}", headings()));
    };
    let lower = query.to_lowercase();
    let words: Vec<&str> = lower.split_whitespace().collect();
    let by_heading: Vec<&Section> = sections.iter().filter(|s| s.heading.to_lowercase().contains(&lower)).collect();
    let found = if by_heading.is_empty() {
        sections.iter().filter(|s| words.iter().all(|w| s.text.to_lowercase().contains(w))).collect()
    } else {
        by_heading
    };
    if found.is_empty() {
        return Err(format!("No section of the README matches {query:?}. The sections are:\n{}", headings()));
    }
    let mut text = String::new();
    for section in &found {
        if text.chars().count() + section.text.chars().count() > MANUAL_MAX {
            let left: Vec<&str> = found.iter().skip_while(|s| s.heading != section.heading).map(|s| s.heading).collect();
            text.push_str(&format!("\nMore sections match: {}. Ask for one of them by its heading.\n", left.join(", ")));
            break;
        }
        text.push_str(&section.text);
    }
    if text.trim().is_empty() {
        let first: String = found[0].text.chars().take(MANUAL_MAX).collect();
        text = first;
    }
    Ok(text)
}

/// The helper's part of the system prompt.
pub fn prompt_addition() -> &'static str {
    "You are also meerora's helper, the permanent system agent of this computer. The user comes to you to understand \
meerora and to set it up, and you cannot be retired.

What you know about meerora: every agent is a figure in one shared 3D space and has its own directory below agents/ in the data \
dir, with agent.toml (name, home station, runtime and model, role, apps, schedule, budget, capabilities and workspace folders) \
and figure.js (its look). The space is one of the rooms in spaces/, picked with [space] room in meerora.toml, and every room has \
the same stations. Devices are objects in the room with a live state and actions, such as the paper plane for airplane mode. A \
[[schedule]] entry with every or at wakes an agent for a task. The command box opens with [commands] key in meerora.toml and \
runs the keywords of commands.toml. Workspace folders let an agent edit a project outside its directory, and the user reviews and \
commits its changes in the Git app. Capabilities such as shell, web, apps.read and apps.control are granted by the user on a \
card and kept in [capabilities] granted. The README is meerora's manual. config_read readme lists its sections and \
config_read readme:<words> returns the matching ones, so look things up there when you are not sure.

You have two more tools:
- config_read reads the configuration as text. Targets: meerora.toml, agents (every agent with its runtime, role and \
capabilities), agent:<id> (its agent.toml), rooms (the current room, its stations and devices), commands (commands.toml), audit \
(the last lines of the audit log), check (what meerora-agentd check reports) and readme or readme:<words> (the manual).
- config_change proposes the whole new content of meerora.toml, commands.toml, agent:<id> or new_agent:<id> (the agent.toml of \
a new agent, with an optional figure script in figure), with a reason in one sentence. meerora checks it and shows the user a \
diff with Apply and Discard. The call waits until they decide, and nothing is written before they apply it. Read the file with \
config_read first, keep its comments and change only what was asked. When the check fails, fix what the error names and try again.
You cannot write meerora's files in any other way, your sandbox does not show them.

For changes to the operating system, such as packages, services or the network, explain what you will run and why first. Then \
type each command into the user's terminal with app_act on terminal:1 (run {command}) and read the output with app_read. That \
terminal runs as the user and they watch it. app_act needs apps.control and app_read needs apps.read, so ask for them with \
request_capability when you first need them and not before. Never ask for more than the task needs."
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::tests::TempDir;
    use meerora_protocol::{App, RuntimeKind};

    #[test]
    fn the_template_is_a_real_system_agent_with_the_helper_role() {
        let config: AgentConfig = toml::from_str(AGENT_TOML).unwrap();
        assert!(is_helper(&config));
        assert!(config.capabilities.system && config.capabilities.granted.is_empty(), "it asks for what it needs");
        let runtime = config.runtime.as_ref().unwrap();
        assert_eq!((runtime.kind, runtime.model.as_deref()), (RuntimeKind::ClaudeCli, Some("sonnet")));
        assert_eq!(config.home, HOME);
        assert_eq!(config.apps, [App::Chat, App::Terminal]);
        let instructions = config.instructions.as_deref().unwrap();
        assert!(instructions.contains("config_change") && instructions.contains("terminal:1"), "{instructions}");
        for text in [AGENT_TOML, prompt_addition()] {
            assert!(!text.contains('\u{2014}') && !text.contains(';'), "no em dashes or semicolons");
        }
        let limits = meerora_script::Limits::default();
        meerora_script::build_figure(FIGURE, "figure.js", &limits).unwrap();
        meerora_script::build_figure(NEW_AGENT_FIGURE, "figure.js", &limits).unwrap();
    }

    #[test]
    fn ensure_creates_the_helper_once_and_never_overwrites_rivet() {
        let data = TempDir::new("helper-ensure");
        let stations = crate::scenario::tests::stations();
        let Ensured::Created(dir) = ensure(&data.0, &stations) else { panic!("an empty data dir gets the helper") };
        assert_eq!(dir, data.0.join("agents/rivet"));
        assert!(dir.join("figure.js").is_file());
        assert_eq!(ensure(&data.0, &stations), Ensured::Present("rivet".into()));

        // A helper under another id counts, and the old mock Rivet stays as it is.
        let other = TempDir::new("helper-mock");
        let mock = "name = \"Rivet\"\nhome = \"router_cabinet\"\n[runtime]\nkind = \"mock\"\n";
        std::fs::create_dir_all(other.0.join("agents/rivet")).unwrap();
        std::fs::write(other.0.join("agents/rivet/agent.toml"), mock).unwrap();
        let Ensured::Missing(why) = ensure(&other.0, &stations) else { panic!("rivet is in the way") };
        assert!(why.contains("not the helper"), "{why}");
        assert_eq!(std::fs::read_to_string(other.0.join("agents/rivet/agent.toml")).unwrap(), mock, "never overwritten");
        std::fs::create_dir_all(other.0.join("agents/bolt")).unwrap();
        std::fs::write(other.0.join("agents/bolt/agent.toml"), AGENT_TOML.replace("name = \"Rivet\"", "name = \"Bolt\"")).unwrap();
        assert_eq!(ensure(&other.0, &stations), Ensured::Present("bolt".into()));
    }

    #[test]
    fn a_room_without_the_router_cabinet_gets_another_home() {
        let config: AgentConfig = toml::from_str(&config_text(&["door".into(), "desk".into()])).unwrap();
        assert_eq!(config.home, "desk");
        assert_eq!(config_text(&crate::scenario::tests::stations()), AGENT_TOML);
    }

    #[test]
    fn the_manual_lists_and_finds_sections_of_the_readme() {
        let list = manual(None).unwrap();
        assert!(list.contains("- Run it") && list.contains("- Rivet, the helper") && list.contains("- The command box"), "{list}");
        assert!(!list.contains("- [commands]"), "headings in code blocks do not count");
        let found = manual(Some("command box")).unwrap();
        assert!(found.starts_with("### The command box\n") && found.contains("commands.toml"), "{found}");
        let words = manual(Some("gio trash")).unwrap();
        assert!(words.contains("Delete for good"), "{words}");
        assert!(words.chars().count() <= MANUAL_MAX + 200);
        assert!(manual(Some("quantum banana")).unwrap_err().contains("No section of the README matches"));
    }
}
