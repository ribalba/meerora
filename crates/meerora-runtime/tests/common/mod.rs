//! Helpers shared by the integration tests.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use meerora_runtime::{McpBridge, Permissions, RunEvent, RunHandle, RunRequest};

/// A fresh, empty directory below the system temp dir.
pub fn scratch(name: &str) -> PathBuf {
    static N: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!("meerora-rt-{name}-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

pub fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

/// Writes an executable script.
///
/// The script is written to a draft and copied into place by `install`, a
/// child process. Writing it here would race with the other tests: a thread
/// that forks while this one holds the file open for writing hands that
/// descriptor to its child until the child calls exec, and executing the
/// script in that moment fails with ETXTBSY ("Text file busy"). A rename
/// would not help, because it keeps the inode the descriptor points to.
/// `install` opens the final file in its own process, which never forks.
pub fn script(path: &Path, body: &str) -> PathBuf {
    let draft = path.with_extension("draft");
    std::fs::write(&draft, body).unwrap();
    let status = std::process::Command::new("install").arg("-m").arg("755").arg(&draft).arg(path).status().unwrap();
    assert!(status.success(), "install {} failed", path.display());
    std::fs::remove_file(&draft).unwrap();
    path.to_path_buf()
}

/// A fake CLI that records its arguments and stdin, then prints `fixture`.
pub fn fake_cli(dir: &Path, fixture: &Path) -> PathBuf {
    let body = format!(
        "#!/bin/sh\nfor a in \"$@\"; do printf '%s\\n' \"$a\"; done > \"{dir}/args\"\ncat > \"{dir}/stdin\"\ncat \"{fixture}\"\n",
        dir = dir.display(),
        fixture = fixture.display()
    );
    script(&dir.join("fake-cli"), &body)
}

/// A fake MCP server (2025-06-18 handshake) with the tools `note` and
/// `go_to`. Every `tools/call` line is appended to `log`.
pub fn fake_mcp(dir: &Path, log: &Path) -> McpBridge {
    let body = format!(
        r#"#!/bin/sh
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id" *: *\([0-9][0-9]*\).*/\1/p')
  case "$line" in
    *'"initialize"'*)
      printf '{{"jsonrpc":"2.0","id":%s,"result":{{"protocolVersion":"2025-06-18","capabilities":{{"tools":{{}}}},"serverInfo":{{"name":"meerora","version":"0"}}}}}}\n' "$id" ;;
    *'"tools/list"'*)
      printf '{{"jsonrpc":"2.0","id":%s,"result":{{"tools":[{{"name":"note","description":"A short progress note","inputSchema":{{"type":"object","properties":{{"text":{{"type":"string"}}}},"required":["text"]}}}},{{"name":"go_to","description":"Walk to a station","inputSchema":{{"type":"object","properties":{{"station":{{"type":"string"}}}}}}}}]}}}}\n' "$id" ;;
    *'"tools/call"'*)
      printf '%s\n' "$line" >> "{log}"
      printf '{{"jsonrpc":"2.0","id":%s,"result":{{"content":[{{"type":"text","text":"noted"}}]}}}}\n' "$id" ;;
    *'"id"'*)
      printf '{{"jsonrpc":"2.0","id":%s,"result":{{}}}}\n' "$id" ;;
  esac
done
"#,
        log = log.display()
    );
    McpBridge { command: script(&dir.join("fake-mcp"), &body), args: Vec::new() }
}

/// The real bridge from this workspace, if it has been built.
pub fn real_bridge(data_dir: &Path, agent: &str) -> Option<McpBridge> {
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target"));
    let command = target.join("debug/meerora-mcp");
    command.is_file().then(|| McpBridge {
        command,
        args: vec![
            "--agent".into(),
            agent.into(),
            "--data-dir".into(),
            data_dir.display().to_string(),
            "--socket".into(),
            data_dir.join("no-daemon.sock").display().to_string(),
        ],
    })
}

pub fn request(dir: &Path, prompt: &str, permissions: Permissions, mcp: Option<McpBridge>) -> RunRequest {
    RunRequest {
        agent: "wren".into(),
        dir: dir.to_path_buf(),
        prompt: prompt.into(),
        system: "You are Wren, a meerora test agent. Keep answers short.".into(),
        session: None,
        model: None,
        permissions,
        mcp,
        isolation: None,
    }
}

/// Events until `Finished`. Panics when the run takes longer than `limit`
/// or sends anything after `Finished`.
pub fn collect(handle: &RunHandle, limit: Duration) -> Vec<RunEvent> {
    let deadline = Instant::now() + limit;
    let mut events = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let event = handle.events.recv_timeout(left).unwrap_or_else(|e| panic!("no Finished after {limit:?} ({e}), got {events:#?}"));
        let done = matches!(event, RunEvent::Finished { .. });
        events.push(event);
        if done {
            break;
        }
    }
    std::thread::sleep(Duration::from_millis(50));
    assert!(handle.events.try_recv().is_err(), "an event followed Finished");
    events
}

pub fn result(events: &[RunEvent]) -> Result<String, String> {
    match events.last() {
        Some(RunEvent::Finished { result }) => result.clone(),
        other => panic!("last event is {other:?}"),
    }
}

pub fn tools(events: &[RunEvent]) -> Vec<(String, String)> {
    events
        .iter()
        .filter_map(|e| match e {
            RunEvent::Tool { tool, summary, .. } => Some((tool.clone(), summary.clone())),
            _ => None,
        })
        .collect()
}

pub fn texts(events: &[RunEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            RunEvent::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

pub fn session(events: &[RunEvent]) -> Option<String> {
    events.iter().find_map(|e| match e {
        RunEvent::Session { id } => Some(id.clone()),
        _ => None,
    })
}

/// With `MEERORA_RECORD_FIXTURES=1`, a wrapper around `real` that copies its
/// stdout to `tests/fixtures/<name>`. Otherwise `real` itself.
pub fn maybe_record(real: &Path, name: &str, dir: &Path) -> PathBuf {
    if std::env::var_os("MEERORA_RECORD_FIXTURES").is_none() {
        return real.to_path_buf();
    }
    let body = format!("#!/bin/sh\n\"{}\" \"$@\" | tee \"{}\"\n", real.display(), fixture(name).display());
    script(&dir.join(format!("record-{name}.sh")), &body)
}

pub fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|d| d.join(name)).find(|p| p.is_file())
}
