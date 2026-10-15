//! Runs the real binary and talks MCP to it over stdin and stdout, with a
//! fake daemon on a temporary socket.

#[path = "../src/testing.rs"]
mod testing;

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command as Process, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use meerora_protocol::{Command, ToolCall};
use serde_json::{Value, json};
use testing::{FakeDaemon, TestDir, answer, write_data_dir};

const BIN: &str = env!("CARGO_BIN_EXE_meerora-mcp");
const TIMEOUT: Duration = Duration::from_secs(10);

/// The running bridge. Killed when the test ends early.
struct Bridge {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: mpsc::Receiver<String>,
}

impl Bridge {
    fn start(dir: &TestDir) -> Self {
        let mut child = Process::new(BIN)
            .args(["--agent", "wren", "--data-dir"])
            .arg(&dir.0)
            .arg("--socket")
            .arg(dir.path("d.sock"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take();
        let stdout: ChildStdout = child.stdout.take().unwrap();
        let (tx, lines) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { return };
                if tx.send(line).is_err() {
                    return;
                }
            }
        });
        Self { child, stdin, lines }
    }

    fn send(&mut self, message: Value) {
        let stdin = self.stdin.as_mut().unwrap();
        writeln!(stdin, "{message}").unwrap();
        stdin.flush().unwrap();
    }

    /// Sends a request and returns the next line from stdout, which must be its reply.
    fn request(&mut self, message: Value) -> Value {
        self.send(message.clone());
        let line = self.lines.recv_timeout(TIMEOUT).expect("a reply on stdout");
        let reply: Value = serde_json::from_str(&line).unwrap_or_else(|e| panic!("stdout carried {line:?}, which is not JSON ({e})"));
        assert_eq!(reply["id"], message["id"], "{reply}");
        reply
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn call(id: u64, tool: &str, arguments: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "method": "tools/call", "params": { "name": tool, "arguments": arguments } })
}

#[test]
fn a_session_over_stdio() {
    let dir = TestDir::new("stdio");
    write_data_dir(&dir);
    let fake = FakeDaemon::start(&dir.path("d.sock"), answer(true, "Walking to the door."));
    let mut bridge = Bridge::start(&dir);

    let init = bridge.request(json!({ "jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {
        "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "stdio-test", "version": "1" } } }));
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(init["result"]["capabilities"]["tools"]["listChanged"], false);
    bridge.send(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));

    let list = bridge.request(json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }));
    let tools = list["result"]["tools"].as_array().unwrap();
    let figure = tools.iter().find(|t| t["name"] == "figure_set").unwrap();
    assert_eq!(figure["inputSchema"]["properties"]["name"]["enum"], json!(["letter"]));
    let go_to = tools.iter().find(|t| t["name"] == "go_to").unwrap();
    assert_eq!(go_to["inputSchema"]["properties"]["station"]["enum"], json!(["desk", "door"]));

    let reply = bridge.request(call(2, "go_to", json!({ "station": "door" })));
    assert_eq!(reply["result"], json!({ "content": [{ "type": "text", "text": "Walking to the door." }], "isError": false }));
    match fake.next_command(TIMEOUT) {
        Some(Command::Tool { agent, call, .. }) => {
            assert_eq!(agent, "wren");
            assert_eq!(call, ToolCall::GoTo { station: "door".into() });
        }
        other => panic!("expected a tool call, got {other:?}"),
    }

    let reply = bridge.request(call(3, "figure_set", json!({ "name": "letter", "value": "yes" })));
    assert_eq!(reply["result"]["isError"], true);
    assert_eq!(fake.next_command(Duration::from_millis(200)), None, "invalid calls are not forwarded");

    let reply = bridge.request(json!({ "jsonrpc": "2.0", "id": 4, "method": "prompts/list" }));
    assert_eq!(reply["error"]["code"], -32601);

    // Closing stdin ends the server.
    drop(bridge.stdin.take());
    let status = bridge.child.wait().unwrap();
    assert!(status.success(), "{status}");
}

#[test]
fn without_a_daemon_calls_fail_but_the_server_keeps_running() {
    let dir = TestDir::new("stdio-nodaemon");
    write_data_dir(&dir);
    let mut bridge = Bridge::start(&dir);
    let reply = bridge.request(call(1, "note", json!({ "text": "hello" })));
    assert_eq!(reply["result"]["isError"], true);
    let text = reply["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.starts_with("meerora is not running, so note had no effect."), "{text}");
    let reply = bridge.request(json!({ "jsonrpc": "2.0", "id": 2, "method": "ping" }));
    assert_eq!(reply["result"], json!({}));
}

#[test]
fn bad_arguments_exit_with_2() {
    let out = Process::new(BIN).arg("--speed").output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
    assert!(String::from_utf8_lossy(&out.stderr).contains("--speed needs a value"));
}
