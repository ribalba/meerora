//! `meerora-agentd init` creates the user's data dir from the shipped
//! assets: the rooms of `spaces/`, Claude on the claude CLI and Rivet, the
//! helper (helper.rs). The demo agents and their scenario stay in the
//! shipped assets, where the mock agents play the demo.

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use meerora_protocol::AgentConfig;

use crate::git::{self, Repo};

/// Never copied from `assets/`, because they belong to one running daemon.
const SKIP: [&str; 5] = [".git", "logs", "artifacts", "state.json", "state.json.tmp"];

/// The sample agent that init copies and switches to a real runtime.
pub const REAL_AGENT: &str = "claude";

/// `meerora.toml` as init writes it.
pub const SETTINGS: &str = r#"# meerora settings. Every value is optional, the comments show the defaults.

# The runtime that writes figure scripts in the builder (screen 3).
# [builder]
# kind = "claude-cli"
# model = "sonnet"

# Where meerora finds the runtimes. Binaries are looked up on PATH when unset.
# [paths]
# claude = "/usr/local/bin/claude"
# codex = "/usr/local/bin/codex"
# ollama = "http://localhost:11434"

# Every real agent runs inside a bubblewrap sandbox that shows only the system, its own directory
# and its CLI home (D16). While it is on, agents never run unsandboxed.
# [sandbox]
# enabled = true
# read = ["~/Documents/shared"]   # more paths every agent may read

# The meerora index where you find external agents. Start a local one with meerora-index serve.
# [index]
# url = "http://127.0.0.1:7878"

# The room of the space (D6): workshop, space-station, office, or custom for a space/ folder from
# before rooms. The room picker of the space card in the sidebar writes it.
# [space]
# room = "workshop"

# Your own apps in agent views get this keyboard layout. While the meerora window has the
# keyboard, it asks the desktop for Alt+Tab, Super and the other desktop shortcuts.
# [desktop]
# keyboard_layout = "us"
# keyboard_variant = ""
# inhibit_shortcuts = true

# Local voice (D16): hold the key to talk, whisper writes the text into the field. Replies can be
# read aloud with Piper. The models (about 300 MB) download on first use into ~/.cache/meerora/voice.
# [voice]
# enabled = true
# key = "F8"
# language = "auto"          # what you speak: auto, en, de, ...
# speak_replies = false
# [voice.voices]
# en = "en_US-lessac-medium"
# de = "de_DE-thorsten-medium"
# [voice.agents.claude]
# speak_replies = true
"#;

/// `$MEERORA_DATA_DIR`, else `$XDG_DATA_HOME/meerora`, else `~/.local/share/meerora`.
pub fn default_target() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("MEERORA_DATA_DIR") {
        return Some(dir.into());
    }
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| Path::new(&home).join(".local/share")))
        .map(|dir| dir.join("meerora"))
}

/// Creates `target` from `assets` and returns what it did, one line each.
pub fn init(target: &Path, assets: &Path, use_git: bool) -> anyhow::Result<Vec<String>> {
    if std::fs::symlink_metadata(target).is_ok() {
        bail!("{} already exists, so init leaves it alone", target.display());
    }
    if !assets.join("spaces").is_dir() {
        bail!("{} has no spaces/ folder to copy", assets.display());
    }
    let claude = assets.join("agents").join(REAL_AGENT);
    if !claude.join("agent.toml").is_file() {
        bail!("{} has no agents/{REAL_AGENT}/ to copy", assets.display());
    }
    std::fs::create_dir_all(target).with_context(|| format!("cannot create {}", target.display()))?;
    let mut lines = vec![format!("created {}", target.display())];
    copy_dir(&assets.join("spaces"), &target.join("spaces"))?;
    lines.push(format!("copied the rooms in spaces/ from {}", assets.display()));
    std::fs::write(target.join("meerora.toml"), SETTINGS).context("cannot write meerora.toml")?;
    lines.push("wrote meerora.toml with commented defaults".into());

    let agents = target.join("agents");
    copy_dir(&claude, &agents.join(REAL_AGENT))?;
    make_real(&agents.join(REAL_AGENT).join("agent.toml"))?;
    lines.push(format!("{REAL_AGENT} runs on claude-cli with the model sonnet"));
    let (stations, _) = crate::space::stations_or_fallback(target);
    let helper = crate::helper::create(target, &stations).map_err(anyhow::Error::msg)?;
    let id = helper.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    lines.push(format!("{id} is meerora's helper, on claude-cli with the model sonnet"));

    if use_git {
        let (mut repos, mut off) = (0, Vec::new());
        for dir in agent_dirs(&target.join("agents"))? {
            match git::ensure_repo(&dir) {
                Repo::On => {
                    repos += 1;
                    if let Err(e) = git::commit(&dir, "meerora init") {
                        off.push(format!("{} has no first commit ({e})", dir.display()));
                    }
                }
                Repo::Off(reason) => off.push(format!("git is off for {} because {reason}", dir.display())),
            }
        }
        lines.push(format!("made {repos} agent directories git repositories"));
        lines.extend(off);
    }
    Ok(lines)
}

fn agent_dirs(agents: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let mut dirs: Vec<PathBuf> =
        std::fs::read_dir(agents)?.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.join("agent.toml").is_file()).collect();
    dirs.sort();
    Ok(dirs)
}

fn copy_dir(from: &Path, to: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(to).with_context(|| format!("cannot create {}", to.display()))?;
    for entry in std::fs::read_dir(from).with_context(|| format!("cannot read {}", from.display()))? {
        let entry = entry?;
        let name = entry.file_name();
        if SKIP.iter().any(|s| name == *s) {
            continue;
        }
        let (source, dest) = (entry.path(), to.join(&name));
        if source.is_dir() {
            copy_dir(&source, &dest)?;
        } else {
            std::fs::copy(&source, &dest).with_context(|| format!("cannot copy {}", source.display()))?;
        }
    }
    Ok(())
}

/// Sets `[runtime]` of an agent.toml to claude-cli with sonnet, keeping comments.
fn make_real(path: &Path) -> anyhow::Result<()> {
    let text = std::fs::read_to_string(path)?;
    let mut lines: Vec<String> = Vec::new();
    let (mut in_runtime, mut found) = (false, false);
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_runtime = trimmed == "[runtime]";
            found |= in_runtime;
        }
        let key = trimmed.split('=').next().unwrap_or("").trim();
        if in_runtime && key == "model" {
            continue;
        }
        if in_runtime && key == "kind" {
            lines.push("kind = \"claude-cli\"".into());
            lines.push("model = \"sonnet\"".into());
            continue;
        }
        lines.push(line.to_string());
    }
    if !found {
        lines.extend(["".into(), "[runtime]".into(), "kind = \"claude-cli\"".into(), "model = \"sonnet\"".into()]);
    }
    let mut new = lines.join("\n");
    new.push('\n');
    toml::from_str::<AgentConfig>(&new).with_context(|| format!("{} would not parse after the change", path.display()))?;
    std::fs::write(path, new)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::tests::TempDir;
    use meerora_protocol::{RuntimeKind, Settings, load_agents};

    #[test]
    fn creates_a_data_dir_with_claude_and_the_helper_only() {
        let tmp = TempDir::new("init");
        let target = tmp.0.join("meerora");
        let assets = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets");
        let lines = init(&target, &assets, true).unwrap();
        assert!(lines.iter().any(|l| l.contains("claude runs on claude-cli")), "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("rivet is meerora's helper")), "{lines:?}");

        for room in ["workshop", "space-station", "office"] {
            assert!(target.join("spaces").join(room).join("space.js").is_file(), "{room}");
        }
        assert_eq!(meerora_protocol::space_dir(&target), target.join("spaces/workshop"), "a new data dir starts in the workshop");
        assert!(!target.join("scenarios").exists(), "the demo scenario stays in the shipped assets");
        assert_eq!(Settings::load(&target).unwrap(), Settings::default());
        let agents: Vec<_> = load_agents(&target).into_iter().map(Result::unwrap).collect();
        assert_eq!(agents.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(), ["claude", "rivet"], "no mock samples");
        for agent in &agents {
            let runtime = agent.config.runtime.as_ref().unwrap();
            assert_eq!((runtime.kind, runtime.model.as_deref()), (RuntimeKind::ClaudeCli, Some("sonnet")), "{}", agent.id);
            assert!(agent.path.join(".git").is_dir(), "{} is a repository", agent.id);
            assert!(agent.path.join(".gitignore").is_file());
        }
        let rivet = &agents[1].config;
        assert!(crate::helper::is_helper(rivet) && rivet.capabilities.system, "the helper has the double violet ring");
        assert!(agents[1].path.join("figure.js").is_file());
        let err = init(&target, &assets, false).unwrap_err().to_string();
        assert!(err.contains("already exists"), "{err}");
    }

    #[test]
    fn make_real_keeps_comments_and_adds_a_missing_table() {
        let tmp = TempDir::new("make-real");
        let path = tmp.0.join("agent.toml");
        std::fs::write(&path, "# a comment\nname = \"C\"\nhome = \"desk\"\n\n[runtime]\nkind = \"mock\"\nmodel = \"x\"\n").unwrap();
        make_real(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("# a comment\n"));
        assert!(text.contains("kind = \"claude-cli\"\nmodel = \"sonnet\"\n") && !text.contains("\"x\""), "{text}");
        std::fs::write(&path, "name = \"C\"\nhome = \"desk\"\n").unwrap();
        make_real(&path).unwrap();
        assert!(std::fs::read_to_string(&path).unwrap().ends_with("[runtime]\nkind = \"claude-cli\"\nmodel = \"sonnet\"\n"));
    }
}
