//! Tool calls in action-log form (D12).
//!
//! Every runtime reports its tool calls through [`action`], so the action log
//! reads the same whichever model is behind an agent.
//!
//! | Action name | Tools |
//! | --- | --- |
//! | `terminal.run` | Bash, shell, exec, run_command |
//! | `editor.patch` | Edit, Write, MultiEdit, apply_patch, write_file |
//! | `files.read` | Read, Glob, Grep, ls, read_file, list_files |
//! | `browser.open` | WebFetch |
//! | `browser.search` | WebSearch, web_search |
//! | `meerora.<name>` | the MCP tools of the meerora server (`mcp__meerora__<name>`) |
//! | `tool.<name>` | everything else |

use std::path::Path;

use serde_json::Value;

use crate::RunEvent;

/// Longest summary in characters, including the ellipsis.
pub const SUMMARY_CHARS: usize = 80;

/// Longest target in characters. A longer one is left out rather than cut,
/// because a cut URL or path would open the wrong thing.
pub const TARGET_CHARS: usize = 8 * 1024;

/// Prefix of the meerora MCP tools as the harness CLIs name them.
pub const MEERORA_MCP_PREFIX: &str = "mcp__meerora__";

/// The [`RunEvent::Tool`] for a call of `tool` with `input`. Paths inside
/// `dir` are shown relative to it.
pub fn action(tool: &str, input: &Value, dir: &Path) -> RunEvent {
    let (tool, summary, target) = action_parts(tool, input, dir);
    RunEvent::Tool { tool, summary, target }
}

/// Action name, summary and target for a call of `tool` with `input`. The
/// target is the full command, path, URL or query, given only when the
/// summary had to be cut or flattened.
pub fn action_parts(tool: &str, input: &Value, dir: &Path) -> (String, String, Option<String>) {
    let name = action_name(tool);
    let (full, has_target) = match name.as_str() {
        "terminal.run" => (command_summary(input), true),
        "editor.patch" => (patch_summary(input, dir), true),
        "files.read" => (read_summary(input, dir), true),
        "browser.open" => (string_field(input, &["url"]).unwrap_or_default(), true),
        "browser.search" => (string_field(input, &["query"]).unwrap_or_default(), true),
        _ => (args_summary(input), false),
    };
    let summary = cut(&full);
    let target = (has_target && summary != full && full.chars().count() <= TARGET_CHARS).then_some(full);
    (name, summary, target)
}

/// The action-log name of a tool.
pub fn action_name(tool: &str) -> String {
    let fixed = match tool {
        "Bash" | "shell" | "exec" | "run_command" | "local_shell" | "shell_command" | "exec_command" => "terminal.run",
        "Edit" | "Write" | "MultiEdit" | "apply_patch" | "write_file" => "editor.patch",
        "Read" | "Glob" | "Grep" | "ls" | "LS" | "read_file" | "list_files" => "files.read",
        "WebFetch" => "browser.open",
        "WebSearch" | "web_search" => "browser.search",
        _ => "",
    };
    if !fixed.is_empty() {
        return fixed.to_string();
    }
    match tool.strip_prefix(MEERORA_MCP_PREFIX) {
        Some(rest) if !rest.is_empty() => format!("meerora.{rest}"),
        _ => format!("tool.{tool}"),
    }
}

/// Collapses whitespace and cuts `text` to [`SUMMARY_CHARS`] characters.
pub fn cut(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= SUMMARY_CHARS {
        return flat;
    }
    let mut short: String = flat.chars().take(SUMMARY_CHARS - 1).collect();
    short.push('…');
    short
}

/// `path` relative to `dir` when it lies inside it, else unchanged.
pub fn display_path(path: &str, dir: &Path) -> String {
    let p = Path::new(path);
    if !p.is_absolute() {
        return path.to_string();
    }
    let canonical = dir.canonicalize().ok();
    for base in std::iter::once(dir).chain(canonical.as_deref()) {
        if let Ok(rest) = p.strip_prefix(base) {
            let rest = rest.to_string_lossy();
            return if rest.is_empty() { ".".to_string() } else { rest.into_owned() };
        }
    }
    path.to_string()
}

fn string_field(input: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|k| input.get(*k).and_then(Value::as_str)).map(str::to_string)
}

fn command_summary(input: &Value) -> String {
    match input.get("command") {
        Some(Value::String(cmd)) => unwrap_shell(cmd),
        Some(Value::Array(parts)) => {
            let parts: Vec<&str> = parts.iter().filter_map(Value::as_str).collect();
            match parts.as_slice() {
                [shell, flag, script] if is_shell(shell) && is_command_flag(flag) => script.to_string(),
                _ => parts.join(" "),
            }
        }
        _ => args_summary(input),
    }
}

fn is_shell(program: &str) -> bool {
    let name = program.rsplit('/').next().unwrap_or(program);
    matches!(name, "sh" | "bash" | "zsh" | "dash")
}

fn is_command_flag(flag: &str) -> bool {
    matches!(flag, "-c" | "-lc" | "-ic")
}

/// `bash -lc 'ls -la'` becomes `ls -la`. Codex reports commands in that form.
fn unwrap_shell(cmd: &str) -> String {
    let trimmed = cmd.trim();
    let mut words = trimmed.splitn(3, ' ');
    if let (Some(shell), Some(flag), Some(script)) = (words.next(), words.next(), words.next())
        && is_shell(shell)
        && is_command_flag(flag)
    {
        let script = script.trim();
        for quote in ['\'', '"'] {
            if let Some(inner) = script.strip_prefix(quote).and_then(|s| s.strip_suffix(quote)) {
                return inner.to_string();
            }
        }
        return script.to_string();
    }
    trimmed.to_string()
}

fn patch_summary(input: &Value, dir: &Path) -> String {
    if let Some(path) = string_field(input, &["file_path", "path", "notebook_path"]) {
        return display_path(&path, dir);
    }
    let mut paths: Vec<String> = Vec::new();
    if let Some(list) = input.get("paths").and_then(Value::as_array) {
        paths.extend(list.iter().filter_map(Value::as_str).map(|p| display_path(p, dir)));
    }
    if let Some(changes) = input.get("changes").and_then(Value::as_array) {
        paths.extend(changes.iter().filter_map(|c| c.get("path").and_then(Value::as_str)).map(|p| display_path(p, dir)));
    }
    if let Some(patch) = string_field(input, &["input", "patch"]) {
        for line in patch.lines() {
            for marker in ["*** Update File: ", "*** Add File: ", "*** Delete File: "] {
                if let Some(p) = line.strip_prefix(marker) {
                    paths.push(display_path(p.trim(), dir));
                }
            }
        }
    }
    if paths.is_empty() { args_summary(input) } else { paths.join(", ") }
}

fn read_summary(input: &Value, dir: &Path) -> String {
    let path = string_field(input, &["file_path", "path"]).map(|p| display_path(&p, dir));
    match (string_field(input, &["pattern"]), path) {
        (Some(pattern), Some(path)) => format!("{pattern} in {path}"),
        (Some(pattern), None) => pattern,
        (None, Some(path)) => path,
        (None, None) => ".".to_string(),
    }
}

/// A single string argument as it is, anything else as compact JSON.
fn args_summary(input: &Value) -> String {
    match input {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Object(map) if map.len() == 1 => match map.values().next() {
            Some(Value::String(s)) => s.clone(),
            _ => input.to_string(),
        },
        Value::Object(map) if map.is_empty() => String::new(),
        _ => input.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn act(tool: &str, input: Value) -> (String, String) {
        let (tool, summary, _) = action_parts(tool, &input, Path::new("/home/u/agents/wren"));
        (tool, summary)
    }

    fn target_of(tool: &str, input: Value) -> Option<String> {
        action_parts(tool, &input, Path::new("/home/u/agents/wren")).2
    }

    fn pair(name: &str, summary: &str) -> (String, String) {
        (name.to_string(), summary.to_string())
    }

    #[test]
    fn claude_tools() {
        assert_eq!(act("Bash", json!({"command": "cargo test", "description": "run tests"})), pair("terminal.run", "cargo test"));
        assert_eq!(act("Edit", json!({"file_path": "/home/u/agents/wren/memory/notes.md"})), pair("editor.patch", "memory/notes.md"));
        assert_eq!(act("Write", json!({"file_path": "/tmp/elsewhere.txt", "content": "x"})), pair("editor.patch", "/tmp/elsewhere.txt"));
        assert_eq!(act("MultiEdit", json!({"file_path": "a.txt", "edits": []})), pair("editor.patch", "a.txt"));
        assert_eq!(act("Read", json!({"file_path": "/home/u/agents/wren/hello.txt"})), pair("files.read", "hello.txt"));
        assert_eq!(act("Glob", json!({"pattern": "**/*.md"})), pair("files.read", "**/*.md"));
        assert_eq!(act("Grep", json!({"pattern": "TODO", "path": "/home/u/agents/wren/src"})), pair("files.read", "TODO in src"));
        assert_eq!(
            act("WebFetch", json!({"url": "https://example.org", "prompt": "summarise"})),
            pair("browser.open", "https://example.org")
        );
        assert_eq!(act("WebSearch", json!({"query": "rust ureq"})), pair("browser.search", "rust ureq"));
    }

    #[test]
    fn meerora_and_other_tools() {
        assert_eq!(act("mcp__meerora__note", json!({"text": "checking mail"})), pair("meerora.note", "checking mail"));
        assert_eq!(
            act("mcp__meerora__figure_set", json!({"name": "letter", "value": true})),
            pair("meerora.figure_set", r#"{"name":"letter","value":true}"#)
        );
        assert_eq!(act("mcp__other__x", json!({})), pair("tool.mcp__other__x", ""));
        assert_eq!(act("TodoWrite", json!({"todos": []})), pair("tool.TodoWrite", r#"{"todos":[]}"#));
    }

    #[test]
    fn codex_and_ollama_tools() {
        assert_eq!(act("exec", json!({"command": "/bin/bash -lc 'cat hello.txt'"})), pair("terminal.run", "cat hello.txt"));
        assert_eq!(act("shell", json!({"command": ["bash", "-lc", "ls -la"]})), pair("terminal.run", "ls -la"));
        assert_eq!(act("shell", json!({"command": ["git", "status"]})), pair("terminal.run", "git status"));
        assert_eq!(
            act(
                "apply_patch",
                json!({"changes": [{"path": "/home/u/agents/wren/a.txt", "kind": "add"}, {"path": "b.txt", "kind": "update"}]})
            ),
            pair("editor.patch", "a.txt, b.txt")
        );
        assert_eq!(
            act("apply_patch", json!({"input": "*** Begin Patch\n*** Update File: src/x.rs\n@@\n*** End Patch"})),
            pair("editor.patch", "src/x.rs")
        );
        assert_eq!(act("ls", json!({})), pair("files.read", "."));
        assert_eq!(act("run_command", json!({"command": "echo hi"})), pair("terminal.run", "echo hi"));
        assert_eq!(act("write_file", json!({"path": "notes/today.md", "content": "x"})), pair("editor.patch", "notes/today.md"));
        assert_eq!(act("list_files", json!({"path": "memory"})), pair("files.read", "memory"));
    }

    #[test]
    fn summaries_are_cut_and_flattened() {
        let long = "x".repeat(200);
        let (_, s) = act("Bash", json!({ "command": long }));
        assert_eq!(s.chars().count(), SUMMARY_CHARS);
        assert!(s.ends_with('…'));
        let (_, s) = act("Bash", json!({"command": "echo a\n  echo b"}));
        assert_eq!(s, "echo a echo b");
        assert_eq!(display_path("/home/u/agents/wren", Path::new("/home/u/agents/wren")), ".");
    }

    #[test]
    fn long_values_keep_their_full_target() {
        let url = format!("https://example.org/{}?q=1", "a/".repeat(60));
        let event = action("WebFetch", &json!({ "url": url }), Path::new("/home/u/agents/wren"));
        let RunEvent::Tool { tool, summary, target } = event else { panic!("a tool event") };
        assert_eq!((tool.as_str(), summary.chars().count()), ("browser.open", SUMMARY_CHARS));
        assert_eq!(target.as_deref(), Some(url.as_str()), "the browser needs the whole URL");

        let deep = format!("/home/u/agents/wren/{}/notes.md", "nested".repeat(15));
        assert_eq!(
            target_of("Edit", json!({ "file_path": deep })),
            Some(format!("{}/notes.md", "nested".repeat(15))),
            "relative like the summary"
        );
        let query = "rust ".repeat(30);
        assert_eq!(target_of("WebSearch", json!({ "query": query })), Some(query.clone()));
        assert_eq!(target_of("Bash", json!({"command": "echo a\n  echo b"})), Some("echo a\n  echo b".into()), "the lines stay");

        assert_eq!(target_of("WebFetch", json!({"url": "https://example.org"})), None, "a short summary is the target");
        assert_eq!(target_of("mcp__meerora__note", json!({ "text": "x".repeat(200) })), None, "arguments are no target");
        let huge = format!("https://example.org/{}", "x".repeat(TARGET_CHARS));
        assert_eq!(target_of("WebFetch", json!({ "url": huge })), None, "too long to keep");
    }
}
