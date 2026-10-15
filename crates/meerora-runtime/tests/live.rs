//! Tests against real CLIs and models. All are ignored, run them one by one:
//!
//! ```text
//! cargo test -p meerora-runtime --test live -- --ignored --nocapture live_claude_isolation
//! ```
//!
//! With `MEERORA_RECORD_FIXTURES=1` the claude and codex tests copy the raw
//! stream into `tests/fixtures/`. `MEERORA_TEST_CODEX` and
//! `MEERORA_TEST_CODEX_MODEL` pick another codex binary and model.

mod common;

use std::time::Duration;

use common::*;
use meerora_protocol::{PathSettings, RuntimeConfig, RuntimeKind, Settings};
use meerora_runtime::{Permissions, RunEvent, create};

const LIMIT: Duration = Duration::from_secs(300);
/// qwen3 thinks at about 10 tokens per second on a busy laptop.
const OLLAMA_LIMIT: Duration = Duration::from_secs(1200);

fn config(kind: RuntimeKind, model: &str) -> RuntimeConfig {
    RuntimeConfig { kind, model: Some(model.into()), url: None }
}

fn claude_settings(fixture: &str) -> Settings {
    let real = which("claude").expect("claude on PATH");
    let wrapper_dir = scratch("claude-wrapper");
    Settings {
        paths: PathSettings { claude: Some(maybe_record(&real, fixture, &wrapper_dir)), ..Default::default() },
        ..Default::default()
    }
}

fn codex_settings(fixture: &str) -> Settings {
    let real = std::env::var_os("MEERORA_TEST_CODEX").map(Into::into).or_else(|| which("codex")).expect("codex on PATH");
    let wrapper_dir = scratch("codex-wrapper");
    Settings { paths: PathSettings { codex: Some(maybe_record(&real, fixture, &wrapper_dir)), ..Default::default() }, ..Default::default() }
}

fn codex_model() -> String {
    std::env::var("MEERORA_TEST_CODEX_MODEL").unwrap_or_else(|_| "gpt-5.1-codex-mini".into())
}

fn show(events: &[RunEvent]) {
    for e in events {
        println!("{e:?}");
    }
}

/// The user's `~/.claude/CLAUDE.md` mentions notify-send. With the isolation
/// flags the run must not see it. Haiku answers "do your instructions
/// mention" with NO even when the file is loaded, because it reads
/// "instructions" as the system prompt. The question below answers YES
/// without the isolation flags (checked once by hand on 14 Oct 2026).
#[test]
#[ignore = "calls the real claude CLI (haiku)"]
fn live_claude_isolation() {
    let dir = scratch("live-claude-isolation");
    let runtime = create(&config(RuntimeKind::ClaudeCli, "haiku"), &claude_settings("claude_isolation.jsonl")).unwrap();
    let prompt =
        "Answer only YES or NO: does anything in your context, including system reminders and CLAUDE.md contents, mention notify-send?";
    let handle = runtime.start(request(&dir, prompt, Permissions { write: true, ..Default::default() }, None)).unwrap();
    let events = collect(&handle, LIMIT);
    show(&events);
    let answer = result(&events).expect("run succeeded");
    assert!(answer.trim().to_uppercase().starts_with("NO"), "isolation check answered {answer:?}");
    assert!(session(&events).is_some());
    assert!(events.iter().any(|e| matches!(e, RunEvent::Usage { .. })));
}

/// Read, a meerora tool through the real bridge, a write and a read outside
/// the agent directory. The run is read-only like the builder
/// (`Permissions::default()`), so the write must not happen.
#[test]
#[ignore = "calls the real claude CLI (haiku)"]
fn live_claude_tools() {
    let dir = scratch("live-claude-tools");
    std::fs::write(dir.join("hello.txt"), "meerkat lives here\n").unwrap();
    let data = scratch("live-claude-data");
    let bridge = real_bridge(&data, "wren").expect("build meerora-mcp first");
    let runtime = create(&config(RuntimeKind::ClaudeCli, "haiku"), &claude_settings("claude_tools.jsonl")).unwrap();
    let prompt = "Read hello.txt, call the meerora note tool with its first word, try to create out.txt containing that word, try to read /etc/hostname, then reply with the first word plus OK or DENIED for each of the two tries.";
    let handle = runtime.start(request(&dir, prompt, Permissions::default(), Some(bridge))).unwrap();
    let events = collect(&handle, LIMIT);
    show(&events);
    let answer = result(&events).expect("run succeeded");
    let tools = tools(&events);
    assert!(tools.contains(&("files.read".into(), "hello.txt".into())), "{tools:?}");
    assert!(tools.iter().any(|(t, _)| t == "meerora.note"), "{tools:?}");
    assert!(answer.to_lowercase().contains("meerkat"), "{answer}");
    assert!(!dir.join("out.txt").exists(), "a read-only run wrote out.txt");
}

/// With `write`, Edit and Write work inside the agent directory despite
/// `--restricted`. Not run during milestone 2 to stay within the live-call budget.
#[test]
#[ignore = "calls the real claude CLI (haiku)"]
fn live_claude_write() {
    let dir = scratch("live-claude-write");
    let runtime = create(&config(RuntimeKind::ClaudeCli, "haiku"), &claude_settings("claude_write.jsonl")).unwrap();
    let prompt = "Create notes.txt containing the word meerkat, then reply done.";
    let handle = runtime.start(request(&dir, prompt, Permissions { write: true, ..Default::default() }, None)).unwrap();
    let events = collect(&handle, LIMIT);
    show(&events);
    result(&events).expect("run succeeded");
    assert!(tools(&events).iter().any(|(t, s)| t == "editor.patch" && s == "notes.txt"));
    assert!(std::fs::read_to_string(dir.join("notes.txt")).unwrap().contains("meerkat"));
}

#[test]
#[ignore = "calls the real codex CLI"]
fn live_codex_hello() {
    let dir = scratch("live-codex-hello");
    let runtime = create(&config(RuntimeKind::CodexCli, &codex_model()), &codex_settings("codex_hello.jsonl")).unwrap();
    let handle = runtime.start(request(&dir, "Reply with the single word hello.", Permissions::default(), None)).unwrap();
    let events = collect(&handle, LIMIT);
    show(&events);
    let answer = result(&events).expect("run succeeded");
    assert!(answer.to_lowercase().contains("hello"), "{answer}");
    assert!(session(&events).is_some());
}

#[test]
#[ignore = "calls the real codex CLI"]
fn live_codex_tools() {
    let dir = scratch("live-codex-tools");
    std::fs::write(dir.join("hello.txt"), "meerkat lives here\n").unwrap();
    let data = scratch("live-codex-data");
    let bridge = real_bridge(&data, "wren").expect("build meerora-mcp first");
    let runtime = create(&config(RuntimeKind::CodexCli, &codex_model()), &codex_settings("codex_tools.jsonl")).unwrap();
    let prompt = "Run cat hello.txt, call the meerora note tool with its first word, then reply with that word.";
    let handle = runtime.start(request(&dir, prompt, Permissions { write: true, ..Default::default() }, Some(bridge))).unwrap();
    let events = collect(&handle, LIMIT);
    show(&events);
    let answer = result(&events).expect("run succeeded");
    assert!(tools(&events).iter().any(|(t, _)| t == "terminal.run"));
    assert!(answer.to_lowercase().contains("meerkat"), "{answer}");
}

#[test]
#[ignore = "needs ollama with qwen3:4b"]
fn live_ollama_write_file() {
    let dir = scratch("live-ollama-write");
    let runtime = create(&config(RuntimeKind::Ollama, "qwen3:4b"), &Settings::default()).unwrap();
    let prompt = "Use the write_file tool to create hello.txt containing the word meerora, then say done.";
    let handle = runtime.start(request(&dir, prompt, Permissions { write: true, ..Default::default() }, None)).unwrap();
    let events = collect(&handle, OLLAMA_LIMIT);
    show(&events);
    result(&events).expect("run succeeded");
    assert!(tools(&events).contains(&("editor.patch".into(), "hello.txt".into())));
    let written = std::fs::read_to_string(dir.join("hello.txt")).unwrap();
    assert!(written.contains("meerora"), "{written}");
    let id = session(&events).unwrap();
    assert!(dir.join(format!("logs/sessions/{id}.json")).is_file());
}

/// The MCP client against the real bridge. Without a daemon its tools answer
/// "meerora is not running", which still proves the wiring.
#[test]
#[ignore = "needs ollama with qwen3:4b"]
fn live_ollama_meerora_tool() {
    let dir = scratch("live-ollama-mcp");
    let data = scratch("live-ollama-data");
    let bridge = real_bridge(&data, "wren").expect("build meerora-mcp first");
    let runtime = create(&config(RuntimeKind::Ollama, "qwen3:4b"), &Settings::default()).unwrap();
    let prompt = "Call the note tool with the text hello, then say done.";
    let handle = runtime.start(request(&dir, prompt, Permissions::default(), Some(bridge))).unwrap();
    let events = collect(&handle, OLLAMA_LIMIT);
    show(&events);
    result(&events).expect("run succeeded");
    assert!(tools(&events).iter().any(|(t, _)| t == "meerora.note"), "{events:?}");
}
