//! The Claude Code CLI as a harness runtime (D22).
//!
//! Runs `claude -p --output-format stream-json --verbose` in the agent
//! directory and reads its event stream. The prompt goes in on stdin.
//!
//! Isolation (D29). The run loads none of the user's settings files
//! (`--setting-sources=` with an empty list, plus `--restricted`), so no
//! user hooks, permission rules, plugins or environment from
//! `~/.claude/settings.json` apply. Memory files are skipped twice: the
//! empty source list turns off `~/.claude/CLAUDE.md` and project memory, and
//! `CLAUDE_CODE_DISABLE_CLAUDE_MDS` turns off all `CLAUDE.md` loading.
//! `--strict-mcp-config` loads only the meerora bridge. Variables of a
//! surrounding Claude Code session are removed from the environment, so a
//! daemon started from one does not link its runs to that session.
//!
//! Permissions (D29). `--tools` names the only built-in tools that exist in
//! the run, `--allowedTools` approves them plus the meerora MCP tools, and
//! `--disallowedTools` denies every other known tool. `--restricted` confines
//! the file tools to the agent directory, and `--permission-prompts none`
//! denies anything that would need a person.
//!
//! Agent runs (with [`RunRequest::isolation`]) get their own
//! `CLAUDE_CONFIG_DIR` with the user's login shared (homes.rs, Q15) and run
//! inside the sandbox (sandbox.rs, D16). The flags above stay as a second line.
//! Their workspace folders come as `--add-dir`, so the file tools reach them.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitStatus;

use meerora_protocol::{RuntimeConfig, Settings};
use serde_json::{Value, json};

use crate::actions::action;
use crate::homes;
use crate::process::{self, ChildSpec, StreamParser};
use crate::sandbox::{self, Mounts};
use crate::{DECISION_WAIT, McpBridge, Permissions, RunEvent, RunHandle, RunRequest, Runtime};

/// Built-in tools of Claude Code 2.1 that an agent never gets.
const ALWAYS_DENIED: &[&str] = &[
    "Agent",
    "Task",
    "TaskOutput",
    "TaskStop",
    "BashOutput",
    "KillShell",
    "KillBash",
    "NotebookEdit",
    "PowerShell",
    "REPL",
    "Skill",
    "SlashCommand",
    "EnterWorktree",
    "ExitWorktree",
    "CronCreate",
    "CronDelete",
    "CronList",
    "RemoteTrigger",
    "Monitor",
];

/// Environment variables that stay, although they start with `CLAUDE`. They
/// carry credentials or pick the API provider, so a run needs them to log in.
pub(crate) const KEPT_ENV: &[&str] = &[
    "CLAUDE_CONFIG_DIR",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
    "CLAUDE_CODE_SKIP_BEDROCK_AUTH",
    "CLAUDE_CODE_SKIP_VERTEX_AUTH",
    "CLAUDE_CODE_SKIP_FOUNDRY_AUTH",
    "CLAUDE_CODE_CLIENT_CERT",
    "CLAUDE_CODE_CLIENT_KEY",
    "CLAUDE_CODE_CLIENT_KEY_PASSPHRASE",
];

pub(crate) struct ClaudeCli {
    binary: Option<PathBuf>,
    model: Option<String>,
}

impl ClaudeCli {
    pub fn new(config: &RuntimeConfig, settings: &Settings) -> Self {
        Self { binary: settings.paths.claude.clone(), model: config.model.clone() }
    }
}

impl Runtime for ClaudeCli {
    fn name(&self) -> &'static str {
        "claude-cli"
    }

    fn start(&self, mut request: RunRequest) -> anyhow::Result<RunHandle> {
        let program = match process::find_binary("claude", self.binary.as_deref()) {
            Ok(p) => p,
            Err(message) => return Ok(process::failed_handle(message)),
        };
        let model = request.model.clone().or_else(|| self.model.clone());
        let mut env_set: Vec<(String, String)> =
            vec![("CLAUDE_CODE_DISABLE_CLAUDE_MDS".into(), "1".into()), ("CLAUDE_CODE_DISABLE_AUTO_MEMORY".into(), "1".into())];
        let mut mounts = None;
        if let Some(isolation) = request.isolation.clone() {
            let dir = homes::dir(&isolation, "claude", &request.agent);
            let user = homes::user_claude_dir();
            let home = match homes::claude(&dir, user.as_deref(), isolation.sandbox) {
                Ok(home) => home,
                Err(message) => return Ok(process::failed_handle(message)),
            };
            request.session = request.session.take().and_then(|s| homes::claude_session(&dir, user.as_deref(), &s));
            env_set.push(("CLAUDE_CONFIG_DIR".into(), dir.display().to_string()));
            if isolation.sandbox {
                let mut m = Mounts::for_run(&isolation, &request.dir, request.mcp.as_ref());
                home.mount(&mut m);
                mounts = Some(m);
            }
        }
        let spec = ChildSpec {
            label: "claude",
            program,
            args: args(&request, model.as_deref()).into_iter().map(OsString::from).collect(),
            dir: request.dir.clone(),
            stdin: request.prompt.clone(),
            env_remove: env_removals(std::env::vars_os().map(|(k, _)| k)),
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
        Ok(process::run_child(spec, ClaudeParser::new(request.dir)))
    }
}

/// Built-in tools the run may use, in the order they are passed.
pub(crate) fn allowed_builtin(p: Permissions) -> Vec<&'static str> {
    let mut tools = vec!["Read", "Glob", "Grep"];
    if p.write {
        tools.extend(["Edit", "Write"]);
    }
    if p.shell {
        tools.push("Bash");
    }
    if p.web {
        tools.extend(["WebFetch", "WebSearch"]);
    }
    tools
}

pub(crate) fn denied(p: Permissions) -> Vec<&'static str> {
    let mut tools: Vec<&'static str> = ALWAYS_DENIED.to_vec();
    if !p.write {
        tools.extend(["Edit", "Write", "MultiEdit"]);
    } else {
        tools.push("MultiEdit");
    }
    if !p.shell {
        tools.push("Bash");
    }
    if !p.web {
        tools.extend(["WebFetch", "WebSearch"]);
    }
    tools
}

/// `--mcp-config` JSON with the one server `meerora`. Its tool calls may
/// take as long as the user needs to decide a capability request (D8).
pub(crate) fn mcp_config(bridge: &McpBridge) -> String {
    json!({
        "mcpServers": {
            "meerora": {
                "type": "stdio",
                "command": bridge.command.to_string_lossy(),
                "args": bridge.args,
                "timeout": DECISION_WAIT.as_millis() as u64,
            }
        }
    })
    .to_string()
}

/// The command line after the program name. Options that take values use
/// the `--name=value` form, so variadic options never swallow the next one
/// and values that start with a dash stay values.
pub(crate) fn args(request: &RunRequest, model: Option<&str>) -> Vec<String> {
    let p = request.permissions;
    let mut args: Vec<String> = vec![
        "-p".into(),
        "--output-format=stream-json".into(),
        "--verbose".into(),
        "--setting-sources=".into(),
        "--restricted".into(),
        "--strict-mcp-config".into(),
        "--disable-slash-commands".into(),
        "--permission-prompts=none".into(),
        format!("--permission-mode={}", if p.write { "acceptEdits" } else { "dontAsk" }),
        format!("--tools={}", allowed_builtin(p).join(",")),
    ];
    // Workspace folders. `--restricted` confines the file tools to the
    // working directory and the folders named with `--add-dir`.
    if let Some(isolation) = &request.isolation {
        args.extend(isolation.workspace.iter().map(|folder| format!("--add-dir={}", folder.display())));
    }
    let mut allowed = allowed_builtin(p);
    if let Some(bridge) = &request.mcp {
        args.push(format!("--mcp-config={}", mcp_config(bridge)));
        // In -p mode claude refuses MCP tools that no allow rule names.
        allowed.extend(["mcp__meerora", "mcp__meerora__*"]);
    }
    args.push(format!("--allowedTools={}", allowed.join(",")));
    args.push(format!("--disallowedTools={}", denied(p).join(",")));
    if !request.system.trim().is_empty() {
        args.push(format!("--append-system-prompt={}", request.system));
    }
    if let Some(model) = model {
        args.push(format!("--model={model}"));
    }
    if let Some(session) = &request.session {
        args.push(format!("--resume={session}"));
    }
    args
}

/// Variables to remove: everything that starts with `CLAUDE` except the
/// login and provider variables in [`KEPT_ENV`].
pub(crate) fn env_removals(keys: impl Iterator<Item = OsString>) -> Vec<OsString> {
    keys.filter(|k| {
        let k = k.to_string_lossy();
        k.starts_with("CLAUDE") && !KEPT_ENV.contains(&k.as_ref())
    })
    .collect()
}

/// Reads `--output-format stream-json`.
pub(crate) struct ClaudeParser {
    dir: PathBuf,
    session: Option<String>,
    result: Option<Result<String, String>>,
    usage: Option<(u64, u64)>,
    last_text: Option<String>,
}

impl ClaudeParser {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir, session: None, result: None, usage: None, last_text: None }
    }

    fn session(&mut self, event: &Value, emit: &mut dyn FnMut(RunEvent)) {
        if self.session.is_none()
            && let Some(id) = event.get("session_id").and_then(Value::as_str)
            && !id.is_empty()
        {
            self.session = Some(id.to_string());
            emit(RunEvent::Session { id: id.to_string() });
        }
    }
}

/// Uncached input (cache writes count, cache reads do not) and output tokens.
fn usage_of(usage: &Value) -> (u64, u64) {
    let n = |k: &str| usage.get(k).and_then(Value::as_u64).unwrap_or(0);
    (n("input_tokens") + n("cache_creation_input_tokens"), n("output_tokens"))
}

impl StreamParser for ClaudeParser {
    fn line(&mut self, line: &str, emit: &mut dyn FnMut(RunEvent)) {
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            tracing::debug!(line, "claude printed a line that is not JSON");
            return;
        };
        match event.get("type").and_then(Value::as_str) {
            Some("system") => {
                if event.get("subtype").and_then(Value::as_str) == Some("init") {
                    self.session(&event, emit);
                }
            }
            Some("assistant") => {
                self.session(&event, emit);
                let nested = event.get("parent_tool_use_id").is_some_and(|v| !v.is_null());
                let blocks = event.pointer("/message/content").and_then(Value::as_array).cloned().unwrap_or_default();
                for block in blocks {
                    match block.get("type").and_then(Value::as_str) {
                        Some("text") if !nested => {
                            let text = block.get("text").and_then(Value::as_str).unwrap_or("").trim();
                            if !text.is_empty() {
                                self.last_text = Some(text.to_string());
                                emit(RunEvent::Text { text: text.to_string() });
                            }
                        }
                        Some("tool_use") => {
                            let name = block.get("name").and_then(Value::as_str).unwrap_or("");
                            let input = block.get("input").cloned().unwrap_or(Value::Null);
                            emit(action(name, &input, &self.dir));
                        }
                        _ => {}
                    }
                }
            }
            Some("result") => {
                self.session(&event, emit);
                if let Some(usage) = event.get("usage") {
                    self.usage = Some(usage_of(usage));
                }
                let subtype = event.get("subtype").and_then(Value::as_str).unwrap_or("");
                let is_error = event.get("is_error").and_then(Value::as_bool).unwrap_or(false);
                let text = event.get("result").and_then(Value::as_str).map(str::trim).unwrap_or("").to_string();
                self.result = Some(if is_error || subtype != "success" { Err(result_error(&event, subtype, &text)) } else { Ok(text) });
            }
            _ => {}
        }
    }

    fn finish(&mut self, status: ExitStatus, stderr: &str, emit: &mut dyn FnMut(RunEvent)) -> Result<String, String> {
        if let Some((input_tokens, output_tokens)) = self.usage {
            emit(RunEvent::Usage { input_tokens, output_tokens });
        }
        if let Some(result) = self.result.take() {
            return result;
        }
        let detail = process::stderr_summary(stderr, |_| false).map(|s| format!(": {s}")).unwrap_or_default();
        Err(format!("claude {} without a result{detail}", process::describe_exit(status)))
    }
}

fn result_error(event: &Value, subtype: &str, text: &str) -> String {
    if !text.is_empty() {
        return format!("claude: {text}");
    }
    let errors: Vec<String> = event
        .get("errors")
        .and_then(Value::as_array)
        .map(|list| list.iter().map(|e| e.as_str().map(str::to_string).unwrap_or_else(|| e.to_string())).collect())
        .unwrap_or_default();
    if !errors.is_empty() {
        return format!("claude: {}", errors.join(" "));
    }
    match subtype {
        "error_max_turns" => "claude stopped at its turn limit".to_string(),
        "" | "success" => "claude reported an error".to_string(),
        other => format!("claude stopped with {other}"),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn request(p: Permissions, mcp: bool) -> RunRequest {
        RunRequest {
            agent: "wren".into(),
            dir: PathBuf::from("/tmp/wren"),
            prompt: "hi".into(),
            system: "You are Wren.".into(),
            session: Some("abc".into()),
            model: None,
            permissions: p,
            mcp: mcp.then(|| McpBridge { command: PathBuf::from("/usr/bin/meerora-mcp"), args: vec!["--agent".into(), "wren".into()] }),
            isolation: None,
        }
    }

    fn list<'a>(args: &'a [String], flag: &str) -> Vec<&'a str> {
        args.iter().find_map(|x| x.strip_prefix(flag)).unwrap().split(',').collect()
    }

    #[test]
    fn command_line_follows_permissions() {
        let a = args(&request(Permissions { write: true, shell: false, web: false }, true), Some("haiku"));
        assert!(a.contains(&"--setting-sources=".to_string()));
        assert!(a.contains(&"--strict-mcp-config".to_string()));
        assert!(a.contains(&"--permission-mode=acceptEdits".to_string()));
        assert!(a.contains(&"--tools=Read,Glob,Grep,Edit,Write".to_string()));
        assert!(a.contains(&"--allowedTools=Read,Glob,Grep,Edit,Write,mcp__meerora,mcp__meerora__*".to_string()));
        let denied = list(&a, "--disallowedTools=");
        assert!(denied.contains(&"Bash") && denied.contains(&"WebFetch") && denied.contains(&"WebSearch"));
        assert!(!denied.contains(&"Edit") && !denied.contains(&"Read"));
        assert!(a.contains(&"--model=haiku".to_string()));
        assert!(a.contains(&"--resume=abc".to_string()));
        assert!(a.contains(&"--append-system-prompt=You are Wren.".to_string()));
        let mcp = a.iter().find(|x| x.starts_with("--mcp-config=")).unwrap();
        let config: Value = serde_json::from_str(mcp.trim_start_matches("--mcp-config=")).unwrap();
        assert_eq!(config["mcpServers"]["meerora"]["command"], "/usr/bin/meerora-mcp");
        assert_eq!(config["mcpServers"]["meerora"]["timeout"], 660_000, "request_capability waits for the user");

        let a = args(&request(Permissions { write: false, shell: true, web: true }, false), None);
        assert!(a.contains(&"--permission-mode=dontAsk".to_string()));
        assert!(a.contains(&"--tools=Read,Glob,Grep,Bash,WebFetch,WebSearch".to_string()));
        assert!(a.contains(&"--strict-mcp-config".to_string()));
        assert!(!a.iter().any(|x| x.starts_with("--mcp-config")));
        let denied = list(&a, "--disallowedTools=");
        assert!(denied.contains(&"Edit") && !denied.contains(&"Bash") && !denied.contains(&"WebFetch"));
    }

    /// The builder runs with `Permissions::default()` and must not write.
    #[test]
    fn read_only_runs_get_no_writing_tool() {
        let a = args(&request(Permissions::default(), true), None);
        assert_eq!(list(&a, "--tools="), vec!["Read", "Glob", "Grep"]);
        assert_eq!(list(&a, "--allowedTools="), vec!["Read", "Glob", "Grep", "mcp__meerora", "mcp__meerora__*"]);
        let denied = list(&a, "--disallowedTools=");
        for tool in ["Edit", "Write", "MultiEdit", "NotebookEdit", "Bash", "WebFetch", "WebSearch", "Agent", "Task"] {
            assert!(denied.contains(&tool), "{tool} is not denied");
        }
        assert!(a.contains(&"--permission-mode=dontAsk".to_string()));
        assert!(a.contains(&"--restricted".to_string()));
    }

    /// Workspace folders reach claude's file tools with one `--add-dir` each.
    #[test]
    fn workspace_folders_are_added_dirs() {
        let mut r = request(Permissions { write: true, shell: false, web: false }, false);
        assert!(!args(&r, None).iter().any(|a| a.starts_with("--add-dir")));
        r.isolation = Some(crate::Isolation {
            workspace: vec![PathBuf::from("/home/u/code/project"), PathBuf::from("/srv/site")],
            ..crate::Isolation::default()
        });
        let a = args(&r, None);
        let added: Vec<&String> = a.iter().filter(|x| x.starts_with("--add-dir")).collect();
        assert_eq!(added, ["--add-dir=/home/u/code/project", "--add-dir=/srv/site"]);
        assert!(a.contains(&"--restricted".to_string()), "the file tools stay confined, now to both");
    }

    #[test]
    fn session_variables_are_removed_but_logins_stay() {
        let keys = ["CLAUDECODE", "CLAUDE_CODE_SESSION_ID", "CLAUDE_CODE_OAUTH_TOKEN", "CLAUDE_CONFIG_DIR", "ANTHROPIC_API_KEY", "PATH"];
        let removed = env_removals(keys.iter().map(OsString::from));
        assert_eq!(removed, vec![OsString::from("CLAUDECODE"), OsString::from("CLAUDE_CODE_SESSION_ID")]);
    }

    #[test]
    fn errors_in_results_are_readable() {
        let mut parser = ClaudeParser::new(Path::new("/tmp").to_path_buf());
        let mut events = Vec::new();
        parser.line(
            r#"{"type":"result","subtype":"error_during_execution","is_error":true,"session_id":"s1","errors":["No conversation found with session ID: x"]}"#,
            &mut |e| events.push(e),
        );
        assert_eq!(events, vec![RunEvent::Session { id: "s1".into() }]);
        assert_eq!(parser.result, Some(Err("claude: No conversation found with session ID: x".into())));
    }
}
