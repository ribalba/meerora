//! The OpenAI Codex CLI as a harness runtime (D22).
//!
//! Runs `codex exec --json` in the agent directory, or `codex exec --json
//! resume <session>` to continue, and reads its JSONL events. The prompt goes
//! in on stdin (`-`).
//!
//! Permissions (D29). Codex runs every command inside its own sandbox:
//! `read-only` without `write`, `workspace-write` (the agent directory) with
//! it. Network access inside the sandbox and the web search tool are only on
//! with `web`. Approvals are off, so nothing waits for a person.
//!
//! Isolation (D29). Codex has no switch to skip `~/.codex/config.toml`, so the
//! run overrides what matters with `-c`:
//! - every MCP server of the user's configuration is disabled by name and
//!   only `meerora` is added
//! - the features in [`DISABLED_FEATURES`] are off, which removes plugins
//!   (and the MCP servers they bring, such as computer use), ChatGPT apps,
//!   browser and computer use, hooks and memories
//! - the `notify` hook is cleared
//! - the agent's system prompt replaces any developer instructions
//!
//! Older versions ignore feature names they do not know. What `-c` cannot
//! reach: execpolicy rules in `~/.codex/rules` (an `allow` rule lets a
//! matching command run outside the sandbox), a global `~/.codex/AGENTS.md`,
//! user skills, and the user's default model, reasoning effort and
//! personality when the agent sets no model.
//!
//! Agent runs (with [`RunRequest::isolation`]) therefore get a `CODEX_HOME`
//! of their own, which meerora writes and which links only the user's
//! `auth.json` (homes.rs, closes Q15). None of the above reaches them, and
//! they run inside the sandbox (sandbox.rs, D16). The `-c` overrides stay.

use std::collections::HashSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;

use meerora_protocol::{RuntimeConfig, Settings};
use serde_json::{Value, json};

use crate::actions::action;
use crate::homes;
use crate::process::{self, ChildSpec, StreamParser};
use crate::sandbox::{self, Mounts};
use crate::{DECISION_WAIT, RunEvent, RunHandle, RunRequest, Runtime};

/// Codex features that an agent run never gets (names of codex 0.162).
pub(crate) const DISABLED_FEATURES: &[&str] = &[
    "plugins",
    "remote_plugin",
    "apps",
    "computer_use",
    "browser_use",
    "browser_use_external",
    "hooks",
    "memories",
    "skill_mcp_dependency_install",
];

pub(crate) struct CodexCli {
    binary: Option<PathBuf>,
    model: Option<String>,
}

impl CodexCli {
    pub fn new(config: &RuntimeConfig, settings: &Settings) -> Self {
        Self { binary: settings.paths.codex.clone(), model: config.model.clone() }
    }
}

impl Runtime for CodexCli {
    fn name(&self) -> &'static str {
        "codex-cli"
    }

    fn start(&self, request: RunRequest) -> anyhow::Result<RunHandle> {
        let program = match process::find_binary("codex", self.binary.as_deref()) {
            Ok(p) => p,
            Err(message) => return Ok(process::failed_handle(message)),
        };
        let model = request.model.clone().or_else(|| self.model.clone());
        let (mut env_set, mut mounts, mut user_servers) = (Vec::new(), None, Vec::new());
        match request.isolation.clone() {
            Some(isolation) => {
                let dir = homes::dir(&isolation, "codex", &request.agent);
                let home = match homes::codex(&dir, homes::user_codex_dir().as_deref(), isolation.sandbox) {
                    Ok(home) => home,
                    Err(message) => return Ok(process::failed_handle(message)),
                };
                env_set.push(("CODEX_HOME".to_string(), dir.display().to_string()));
                if isolation.sandbox {
                    let mut m = Mounts::for_run(&isolation, &request.dir, request.mcp.as_ref());
                    home.mount(&mut m);
                    mounts = Some(m);
                }
            }
            // In the user's own home, switch off the MCP servers it configures.
            None => user_servers = user_mcp_servers(&codex_home()),
        }
        let spec = ChildSpec {
            label: "codex",
            program,
            args: args(&request, model.as_deref(), &user_servers).into_iter().map(OsString::from).collect(),
            dir: request.dir.clone(),
            stdin: request.prompt.clone(),
            env_remove: Vec::new(),
            env_set,
            sandboxed: false,
        };
        let spec = match mounts {
            None => spec,
            Some(m) => match sandbox::wrap(spec, m) {
                Ok(spec) => spec,
                Err(message) => return Ok(process::failed_handle(message)),
            },
        };
        Ok(process::run_child(spec, CodexParser::new(request.dir)))
    }
}

/// `$CODEX_HOME`, else `~/.codex`.
fn codex_home() -> PathBuf {
    if let Some(home) = std::env::var_os("CODEX_HOME") {
        return PathBuf::from(home);
    }
    std::env::var_os("HOME").map(|h| Path::new(&h).join(".codex")).unwrap_or_else(|| PathBuf::from(".codex"))
}

/// Names of the MCP servers in the user's codex configuration, which the run
/// disables. Names that a `-c` path cannot address are skipped with a warning.
pub(crate) fn user_mcp_servers(codex_home: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(codex_home.join("config.toml")) else { return Vec::new() };
    let table: toml::Table = match toml::from_str(&text) {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!("cannot read the codex configuration to disable its MCP servers: {e}");
            return Vec::new();
        }
    };
    let Some(servers) = table.get("mcp_servers").and_then(toml::Value::as_table) else { return Vec::new() };
    servers
        .keys()
        .filter(|name| *name != "meerora")
        .filter(|name| {
            let plain = name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
            if !plain {
                tracing::warn!("codex MCP server {name:?} cannot be disabled with -c and may load in agent runs");
            }
            plain
        })
        .cloned()
        .collect()
}

/// A TOML string literal. JSON string escapes are valid TOML basic strings.
fn toml_string(s: &str) -> String {
    Value::String(s.to_string()).to_string()
}

/// The command line after the program name.
pub(crate) fn args(request: &RunRequest, model: Option<&str>, user_servers: &[String]) -> Vec<String> {
    let p = request.permissions;
    let mut args: Vec<String> = vec![
        "exec".into(),
        "--json".into(),
        "--skip-git-repo-check".into(),
        "-C".into(),
        request.dir.to_string_lossy().into_owned(),
        "-s".into(),
        if p.write { "workspace-write" } else { "read-only" }.into(),
    ];
    if let Some(model) = model {
        args.extend(["-m".into(), model.to_string()]);
    }
    // Workspace folders are writable next to the agent directory. Without
    // `write` codex reads them anyway, its read-only sandbox reads everywhere.
    if p.write
        && let Some(isolation) = &request.isolation
    {
        for folder in &isolation.workspace {
            args.extend(["--add-dir".into(), folder.to_string_lossy().into_owned()]);
        }
    }
    let mut config = vec![
        "approval_policy=\"never\"".to_string(),
        format!("web_search={}", if p.web { "\"live\"" } else { "\"disabled\"" }),
        format!("sandbox_workspace_write.network_access={}", p.web),
        "notify=[]".to_string(),
    ];
    config.extend(DISABLED_FEATURES.iter().map(|f| format!("features.{f}=false")));
    if !request.system.trim().is_empty() {
        config.push(format!("developer_instructions={}", toml_string(&request.system)));
    }
    for name in user_servers {
        config.push(format!("mcp_servers.{name}.enabled=false"));
    }
    if let Some(bridge) = &request.mcp {
        let command = toml_string(&bridge.command.to_string_lossy());
        let bridge_args = json!(bridge.args).to_string();
        // request_capability waits for the user (D8), codex waits 60 s by default.
        let timeout = DECISION_WAIT.as_secs();
        config.push(format!("mcp_servers.meerora={{command={command},args={bridge_args},enabled=true,tool_timeout_sec={timeout}}}"));
    }
    for c in config {
        args.extend(["-c".into(), c]);
    }
    if let Some(session) = &request.session {
        args.extend(["resume".into(), session.clone()]);
    }
    args.push("-".into());
    args
}

/// Reads the JSONL events of `codex exec --json`.
pub(crate) struct CodexParser {
    dir: PathBuf,
    session_sent: bool,
    reported: HashSet<String>,
    last_message: Option<String>,
    usage: Option<(u64, u64)>,
    completed: bool,
    failed: Option<String>,
    last_error: Option<String>,
}

impl CodexParser {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            session_sent: false,
            reported: HashSet::new(),
            last_message: None,
            usage: None,
            completed: false,
            failed: None,
            last_error: None,
        }
    }

    fn item(&mut self, item: &Value, completed: bool, emit: &mut dyn FnMut(RunEvent)) {
        let kind = item.get("type").and_then(Value::as_str).unwrap_or("");
        let id = item.get("id").and_then(Value::as_str).unwrap_or("").to_string();
        let call = match kind {
            "agent_message" if completed => {
                let text = item.get("text").and_then(Value::as_str).unwrap_or("").trim();
                if !text.is_empty() {
                    self.last_message = Some(text.to_string());
                    emit(RunEvent::Text { text: text.to_string() });
                }
                None
            }
            "command_execution" => Some(("exec".to_string(), json!({ "command": item.get("command").cloned().unwrap_or(Value::Null) }))),
            "mcp_tool_call" => {
                let server = item.get("server").and_then(Value::as_str).unwrap_or("");
                let tool = item.get("tool").and_then(Value::as_str).unwrap_or("");
                Some((format!("mcp__{server}__{tool}"), item.get("arguments").cloned().unwrap_or(Value::Null)))
            }
            // These two only carry their content once they completed.
            "file_change" if completed => Some(("apply_patch".to_string(), json!({ "changes": item.get("changes").cloned() }))),
            "web_search" if completed => Some(("web_search".to_string(), json!({ "query": item.get("query").cloned() }))),
            _ => None,
        };
        if let Some((name, input)) = call
            && self.reported.insert(if id.is_empty() { format!("{kind}:{}", self.reported.len()) } else { id })
        {
            emit(action(&name, &input, &self.dir));
        }
    }
}

/// Codex wraps API errors as JSON, for example `{"detail":"..."}`.
fn readable(message: &str) -> String {
    if let Ok(v) = serde_json::from_str::<Value>(message) {
        for key in ["detail", "message", "error"] {
            match v.get(key) {
                Some(Value::String(s)) => return s.clone(),
                Some(Value::Object(o)) => {
                    if let Some(Value::String(s)) = o.get("message") {
                        return s.clone();
                    }
                }
                _ => {}
            }
        }
    }
    message.to_string()
}

impl StreamParser for CodexParser {
    fn line(&mut self, line: &str, emit: &mut dyn FnMut(RunEvent)) {
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            tracing::debug!(line, "codex printed a line that is not JSON");
            return;
        };
        match event.get("type").and_then(Value::as_str).unwrap_or("") {
            "thread.started" => {
                if let Some(id) = event.get("thread_id").and_then(Value::as_str)
                    && !self.session_sent
                {
                    self.session_sent = true;
                    emit(RunEvent::Session { id: id.to_string() });
                }
            }
            kind @ ("item.started" | "item.updated" | "item.completed") => {
                if let Some(item) = event.get("item") {
                    self.item(item, kind == "item.completed", emit);
                }
            }
            "turn.completed" => {
                self.completed = true;
                if let Some(u) = event.get("usage") {
                    let n = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
                    let (input, output) = self.usage.unwrap_or((0, 0));
                    let uncached = n("input_tokens").saturating_sub(n("cached_input_tokens"));
                    self.usage = Some((input + uncached, output + n("output_tokens")));
                }
            }
            "turn.failed" => {
                let message = event.pointer("/error/message").and_then(Value::as_str).unwrap_or("the turn failed");
                self.failed = Some(readable(message));
            }
            "error" => {
                if let Some(message) = event.get("message").and_then(Value::as_str) {
                    self.last_error = Some(readable(message));
                }
            }
            _ => {}
        }
    }

    fn finish(&mut self, status: ExitStatus, stderr: &str, emit: &mut dyn FnMut(RunEvent)) -> Result<String, String> {
        if let Some((input_tokens, output_tokens)) = self.usage {
            emit(RunEvent::Usage { input_tokens, output_tokens });
        }
        if let Some(message) = self.failed.take() {
            return Err(format!("codex: {message}"));
        }
        if self.completed {
            return Ok(self.last_message.take().unwrap_or_default());
        }
        if let Some(message) = self.last_error.take() {
            return Err(format!("codex: {message}"));
        }
        // Codex logs model cache warnings on stderr. They say nothing about this run.
        let detail = process::stderr_summary(stderr, |l| l.contains(" WARN ") || l.contains("models_manager"))
            .map(|s| format!(": {s}"))
            .unwrap_or_default();
        Err(format!("codex {} without finishing the turn{detail}", process::describe_exit(status)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{McpBridge, Permissions};

    fn request(p: Permissions, session: Option<&str>) -> RunRequest {
        RunRequest {
            agent: "wren".into(),
            dir: PathBuf::from("/tmp/wren"),
            prompt: "hi".into(),
            system: "You are \"Wren\".\nBe brief.".into(),
            session: session.map(str::to_string),
            model: None,
            permissions: p,
            mcp: Some(McpBridge { command: PathBuf::from("/usr/bin/meerora-mcp"), args: vec!["--agent".into(), "wren".into()] }),
            isolation: None,
        }
    }

    #[test]
    fn command_line_follows_permissions() {
        let a = args(&request(Permissions { write: true, shell: false, web: false }, None), Some("gpt-x"), &["node_repl".into()]);
        let joined = a.join(" ");
        assert!(joined.starts_with("exec --json --skip-git-repo-check -C /tmp/wren -s workspace-write -m gpt-x"));
        assert!(a.contains(&"web_search=\"disabled\"".to_string()));
        assert!(a.contains(&"sandbox_workspace_write.network_access=false".to_string()));
        assert!(a.contains(&"mcp_servers.node_repl.enabled=false".to_string()));
        for feature in ["plugins", "apps", "computer_use", "hooks"] {
            assert!(a.contains(&format!("features.{feature}=false")), "{feature} stays on");
        }
        assert!(a.contains(&r#"developer_instructions="You are \"Wren\".\nBe brief.""#.to_string()));
        assert!(
            a.contains(
                &r#"mcp_servers.meerora={command="/usr/bin/meerora-mcp",args=["--agent","wren"],enabled=true,tool_timeout_sec=660}"#
                    .to_string()
            )
        );
        assert_eq!(a.last().unwrap(), "-");
        assert!(!a.contains(&"resume".to_string()));

        let a = args(&request(Permissions { write: false, shell: true, web: true }, Some("t1")), None, &[]);
        assert!(a.contains(&"read-only".to_string()));
        assert!(a.contains(&"web_search=\"live\"".to_string()));
        let n = a.len();
        assert_eq!(&a[n - 3..], &["resume".to_string(), "t1".to_string(), "-".to_string()]);
    }

    #[test]
    fn workspace_folders_are_writable_dirs() {
        let mut r = request(Permissions { write: true, shell: false, web: false }, None);
        r.isolation = Some(crate::Isolation { workspace: vec![PathBuf::from("/home/u/code/project")], ..crate::Isolation::default() });
        let a = args(&r, None, &[]);
        let at = a.iter().position(|x| x == "--add-dir").expect("the folder is added");
        assert_eq!(a[at + 1], "/home/u/code/project");
        assert!(at < a.iter().position(|x| x == "-c").unwrap(), "before the config overrides");
        r.permissions.write = false;
        assert!(!args(&r, None, &[]).contains(&"--add-dir".to_string()), "a read-only run writes nowhere");
    }

    #[test]
    fn developer_instructions_parse_as_toml() {
        let system = "Line one.\n\"quoted\" and back\\slash and tab\t and unicode ü";
        let line = format!("developer_instructions={}", toml_string(system));
        let table: toml::Table = toml::from_str(&line).unwrap();
        assert_eq!(table["developer_instructions"].as_str(), Some(system));
        let bridge = r#"mcp_servers.meerora={command="/x y/meerora-mcp",args=["--agent","w"],enabled=true,tool_timeout_sec=660}"#;
        let table: toml::Table = toml::from_str(bridge).unwrap();
        assert_eq!(table["mcp_servers"]["meerora"]["args"][1].as_str(), Some("w"));
    }

    #[test]
    fn user_servers_come_from_config() {
        let dir = std::env::temp_dir().join(format!("meerora-codex-home-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.toml"), "model = \"x\"\n[mcp_servers.node_repl]\ncommand = \"n\"\n[mcp_servers.\"a.b\"]\ncommand = \"m\"\n[mcp_servers.meerora]\ncommand = \"z\"\n").unwrap();
        assert_eq!(user_mcp_servers(&dir), vec!["node_repl".to_string()]);
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(user_mcp_servers(Path::new("/nonexistent/codex")).is_empty());
    }

    #[test]
    fn api_errors_are_unwrapped() {
        assert_eq!(readable(r#"{"detail":"The 'x' model is not supported."}"#), "The 'x' model is not supported.");
        assert_eq!(readable("plain"), "plain");
    }
}
