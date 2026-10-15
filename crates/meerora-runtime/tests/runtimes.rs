//! Offline tests of the whole `start` path. The claude and codex streams are
//! real recordings (see `tests/live.rs`) printed by a fake binary, and
//! Ollama is a small HTTP server on a local port.

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::*;
use meerora_protocol::{PathSettings, RuntimeConfig, RuntimeKind, Settings};
use meerora_runtime::{Permissions, RunEvent, create};
use serde_json::{Value, json};

const LIMIT: Duration = Duration::from_secs(20);

fn config(kind: RuntimeKind, model: Option<&str>) -> RuntimeConfig {
    RuntimeConfig { kind, model: model.map(str::to_string), url: None }
}

fn claude_at(path: &Path) -> Settings {
    Settings { paths: PathSettings { claude: Some(path.to_path_buf()), ..Default::default() }, ..Default::default() }
}

fn codex_at(path: &Path) -> Settings {
    Settings { paths: PathSettings { codex: Some(path.to_path_buf()), ..Default::default() }, ..Default::default() }
}

/// Copies a recorded stream with the recorded agent directory replaced by `dir`.
fn relocate(fixture_name: &str, recorded_dir: &str, dir: &Path, out: &Path) -> PathBuf {
    let text = std::fs::read_to_string(fixture(fixture_name)).unwrap();
    let target = out.join(fixture_name);
    std::fs::write(&target, text.replace(recorded_dir, &dir.display().to_string())).unwrap();
    target
}

fn recorded_cwd(fixture_name: &str) -> String {
    let text = std::fs::read_to_string(fixture(fixture_name)).unwrap();
    let init: Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
    init["cwd"].as_str().unwrap().to_string()
}

fn lines(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path).unwrap().lines().map(str::to_string).collect()
}

#[test]
fn claude_stream_with_tools() {
    let dir = scratch("claude-agent");
    let bin = scratch("claude-bin");
    let stream = relocate("claude_tools.jsonl", &recorded_cwd("claude_tools.jsonl"), &dir, &bin);
    let fake = fake_cli(&bin, &stream);
    let runtime = create(&config(RuntimeKind::ClaudeCli, Some("haiku")), &claude_at(&fake)).unwrap();
    assert_eq!(runtime.name(), "claude-cli");
    let events = collect(&runtime.start(request(&dir, "Read hello.txt", Permissions::default(), None)).unwrap(), LIMIT);

    assert_eq!(events[0], RunEvent::Session { id: "413a3e2b-daf1-44c9-8169-722233e0d69e".into() });
    assert_eq!(
        tools(&events),
        vec![
            ("files.read".to_string(), "hello.txt".to_string()),
            ("files.read".to_string(), "/etc/hostname".to_string()),
            ("meerora.note".to_string(), "meerkat".to_string()),
        ]
    );
    let answer = result(&events).unwrap();
    assert!(answer.starts_with("meerkat\n\n- **Create out.txt:** DENIED."), "{answer}");
    assert_eq!(texts(&events), vec![answer.clone()]);
    assert_eq!(events[events.len() - 2], RunEvent::Usage { input_tokens: 6982, output_tokens: 528 });

    let args = lines(&bin.join("args"));
    for expected in ["-p", "--output-format=stream-json", "--verbose", "--setting-sources=", "--restricted", "--strict-mcp-config"] {
        assert!(args.contains(&expected.to_string()), "{expected} missing in {args:?}");
    }
    assert!(args.contains(&"--permission-mode=dontAsk".to_string()));
    assert!(args.contains(&"--tools=Read,Glob,Grep".to_string()));
    assert!(args.contains(&"--model=haiku".to_string()));
    assert_eq!(std::fs::read_to_string(bin.join("stdin")).unwrap(), "Read hello.txt");
}

#[test]
fn claude_runs_without_the_users_session_variables() {
    let dir = scratch("claude-env");
    let bin = scratch("claude-env-bin");
    let body = format!(
        "#!/bin/sh\nenv > \"{}/env\"\ncat \"{}\"\n",
        bin.display(),
        relocate("claude_isolation.jsonl", &recorded_cwd("claude_isolation.jsonl"), &dir, &bin).display()
    );
    let fake = script(&bin.join("fake-claude"), &body);
    let runtime = create(&config(RuntimeKind::ClaudeCli, None), &claude_at(&fake)).unwrap();
    let events = collect(&runtime.start(request(&dir, "q", Permissions::default(), None)).unwrap(), LIMIT);
    assert_eq!(result(&events), Ok("NO".into()));
    assert_eq!(events[events.len() - 2], RunEvent::Usage { input_tokens: 633, output_tokens: 4 });

    let env = lines(&bin.join("env"));
    assert!(env.contains(&"CLAUDE_CODE_DISABLE_CLAUDE_MDS=1".to_string()));
    assert!(env.contains(&"CLAUDE_CODE_DISABLE_AUTO_MEMORY=1".to_string()));
    let kept = [
        "CLAUDE_CODE_DISABLE_CLAUDE_MDS",
        "CLAUDE_CODE_DISABLE_AUTO_MEMORY",
        "CLAUDE_CONFIG_DIR",
        "CLAUDE_CODE_OAUTH_TOKEN",
        "CLAUDE_CODE_USE_",
    ];
    for line in env.iter().filter(|l| l.starts_with("CLAUDE")) {
        assert!(kept.iter().any(|k| line.starts_with(k)), "{line} leaked into the run");
    }
}

#[test]
fn codex_stream_with_tools() {
    let dir = scratch("codex-agent");
    let bin = scratch("codex-bin");
    let fake = fake_cli(&bin, &fixture("codex_tools.jsonl"));
    let runtime = create(&config(RuntimeKind::CodexCli, Some("gpt-6-luna")), &codex_at(&fake)).unwrap();
    assert_eq!(runtime.name(), "codex-cli");
    let events = collect(
        &runtime.start(request(&dir, "Run cat hello.txt", Permissions { write: true, ..Default::default() }, None)).unwrap(),
        LIMIT,
    );

    assert_eq!(
        events,
        vec![
            RunEvent::Session { id: "01a139fa-8315-7480-af24-3c1e22ec0c0c".into() },
            RunEvent::Text { text: "I’ll read `hello.txt`, then use its first word with the Meerora note tool.".into() },
            RunEvent::Tool { tool: "terminal.run".into(), summary: "cat hello.txt".into(), target: None },
            RunEvent::Tool { tool: "meerora.note".into(), summary: "meerkat".into(), target: None },
            RunEvent::Text { text: "meerkat".into() },
            RunEvent::Usage { input_tokens: 40900 - 29952, output_tokens: 338 },
            RunEvent::Finished { result: Ok("meerkat".into()) },
        ]
    );
    let args = lines(&bin.join("args"));
    assert_eq!(&args[..3], &["exec", "--json", "--skip-git-repo-check"]);
    assert!(args.windows(2).any(|w| w == ["-s", "workspace-write"]));
    assert!(args.windows(2).any(|w| w == ["-m", "gpt-6-luna"]));
    assert_eq!(args.last().unwrap(), "-");
    assert!(!args.contains(&"resume".to_string()));
    assert_eq!(std::fs::read_to_string(bin.join("stdin")).unwrap(), "Run cat hello.txt");
}

#[test]
fn codex_resumes_and_reports_refused_models() {
    let dir = scratch("codex-resume");
    let bin = scratch("codex-resume-bin");
    let fake = fake_cli(&bin, &fixture("codex_rejected.jsonl"));
    let runtime = create(&config(RuntimeKind::CodexCli, None), &codex_at(&fake)).unwrap();
    let mut req = request(&dir, "hi", Permissions::default(), None);
    req.session = Some("01a139f9-0000-7000-8000-000000000000".into());
    let events = collect(&runtime.start(req).unwrap(), LIMIT);
    assert_eq!(session(&events).as_deref(), Some("01a139f9-e9c0-72d2-9a1f-b96f4bda82ef"));
    assert_eq!(
        result(&events),
        Err("codex: The 'gpt-5.1-codex-mini' model is not supported when using Codex with a ChatGPT account.".into())
    );
    let args = lines(&bin.join("args"));
    let n = args.len();
    assert_eq!(&args[n - 3..], &["resume", "01a139f9-0000-7000-8000-000000000000", "-"]);
    assert!(args.windows(2).any(|w| w == ["-s", "read-only"]));
}

#[test]
fn codex_hello_from_a_recording() {
    let dir = scratch("codex-hello");
    let bin = scratch("codex-hello-bin");
    let fake = fake_cli(&bin, &fixture("codex_hello.jsonl"));
    let runtime = create(&config(RuntimeKind::CodexCli, None), &codex_at(&fake)).unwrap();
    let events = collect(&runtime.start(request(&dir, "hi", Permissions::default(), None)).unwrap(), LIMIT);
    assert_eq!(result(&events), Ok("hello".into()));
    assert_eq!(texts(&events), vec!["hello".to_string()]);
}

#[test]
fn missing_and_broken_binaries_are_readable_errors() {
    let dir = scratch("missing");
    let run = |kind, settings: Settings| {
        let runtime = create(&config(kind, None), &settings).unwrap();
        result(&collect(&runtime.start(request(&dir, "hi", Permissions::default(), None)).unwrap(), LIMIT))
    };
    assert_eq!(
        run(RuntimeKind::ClaudeCli, claude_at(Path::new("/nonexistent/claude"))),
        Err("claude is not installed (looked for /nonexistent/claude)".into())
    );
    assert_eq!(
        run(RuntimeKind::ClaudeCli, claude_at(Path::new("meerora-test-no-such-claude"))),
        Err("claude is not installed (looked for meerora-test-no-such-claude on PATH)".into())
    );
    assert_eq!(
        run(RuntimeKind::CodexCli, codex_at(Path::new("/nonexistent/codex"))),
        Err("codex is not installed (looked for /nonexistent/codex)".into())
    );

    let bin = scratch("broken-bin");
    let broken = script(&bin.join("claude"), "#!/bin/sh\necho 'Error: not logged in' >&2\nexit 2\n");
    assert_eq!(
        run(RuntimeKind::ClaudeCli, claude_at(&broken)),
        Err("claude exited with status 2 without a result: Error: not logged in".into())
    );
    let broken = script(&bin.join("codex"), "#!/bin/sh\necho 'Error: unexpected argument' >&2\nexit 2\n");
    assert_eq!(
        run(RuntimeKind::CodexCli, codex_at(&broken)),
        Err("codex exited with status 2 without finishing the turn: Error: unexpected argument".into())
    );

    let runtime = create(&config(RuntimeKind::ClaudeCli, None), &claude_at(&broken)).unwrap();
    let gone = dir.join("does-not-exist");
    let events = collect(&runtime.start(request(&gone, "hi", Permissions::default(), None)).unwrap(), LIMIT);
    assert_eq!(result(&events), Err(format!("agent directory {} does not exist", gone.display())));
}

#[test]
fn other_kinds_have_no_runtime() {
    for kind in [RuntimeKind::Api, RuntimeKind::Mock] {
        assert!(create(&config(kind, None), &Settings::default()).is_err());
    }
    // External agents run on the A2A client (D25, Q9).
    assert_eq!(create(&config(RuntimeKind::External, None), &Settings::default()).unwrap().name(), "external");
}

fn pid_alive(pid: u32) -> bool {
    // A zombie still has a /proc entry, so look at its state.
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => !stat.split(' ').nth(2).is_some_and(|state| state == "Z"),
        Err(_) => false,
    }
}

#[test]
fn cancel_kills_the_process_group() {
    let dir = scratch("cancel");
    let bin = scratch("cancel-bin");
    let body = format!(
        "#!/bin/sh\necho '{{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s-cancel\"}}'\nsleep 30 &\necho $! > \"{}/child\"\nwait\n",
        bin.display()
    );
    let fake = script(&bin.join("claude"), &body);
    let runtime = create(&config(RuntimeKind::ClaudeCli, None), &claude_at(&fake)).unwrap();
    let handle = runtime.start(request(&dir, "hi", Permissions::default(), None)).unwrap();
    assert_eq!(handle.events.recv_timeout(LIMIT).unwrap(), RunEvent::Session { id: "s-cancel".into() });
    // The shell creates the file before it writes the pid, so wait for the pid.
    let deadline = Instant::now() + LIMIT;
    let child: u32 = loop {
        if let Some(pid) = std::fs::read_to_string(bin.join("child")).ok().and_then(|t| t.trim().parse().ok()) {
            break pid;
        }
        assert!(Instant::now() < deadline, "no pid in {}", bin.join("child").display());
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(pid_alive(child));

    let canceller = handle.canceller();
    let started = Instant::now();
    std::thread::spawn(move || canceller.cancel()).join().unwrap();
    let events = collect(&handle, LIMIT);
    assert_eq!(events, vec![RunEvent::Finished { result: Err("cancelled".into()) }]);
    assert!(started.elapsed() < Duration::from_secs(2));
    while pid_alive(child) {
        assert!(started.elapsed() < Duration::from_secs(5), "sleep {child} survived the cancel");
        std::thread::sleep(Duration::from_millis(20));
    }
    handle.cancel();
}

#[test]
fn leftover_children_do_not_hold_the_run() {
    let dir = scratch("leftover");
    let bin = scratch("leftover-bin");
    let stream = relocate("claude_isolation.jsonl", &recorded_cwd("claude_isolation.jsonl"), &dir, &bin);
    // The background sleep inherits stdout, so the pipe stays open after exit.
    let body = format!("#!/bin/sh\ncat \"{}\"\nsleep 30 &\necho $! > \"{}/child\"\nexit 0\n", stream.display(), bin.display());
    let fake = script(&bin.join("claude"), &body);
    let runtime = create(&config(RuntimeKind::ClaudeCli, None), &claude_at(&fake)).unwrap();
    let started = Instant::now();
    let events = collect(&runtime.start(request(&dir, "q", Permissions::default(), None)).unwrap(), LIMIT);
    assert_eq!(result(&events), Ok("NO".into()));
    // Half the sleep proves that the run did not wait for it and leaves
    // room for a machine busy with builds.
    let bound = Duration::from_secs(15);
    assert!(started.elapsed() < bound, "took {:?}", started.elapsed());
    let child: u32 = std::fs::read_to_string(bin.join("child")).unwrap().trim().parse().unwrap();
    while pid_alive(child) {
        assert!(started.elapsed() < bound, "sleep {child} outlived the run");
        std::thread::sleep(Duration::from_millis(20));
    }
}

// ---------------------------------------------------------------- Ollama ---

#[derive(Clone)]
enum Reply {
    /// 200 with these NDJSON chunks.
    Stream(Vec<Value>),
    Status(u16, String),
    /// 200, one chunk, then silence.
    Hang(Value),
}

struct FakeOllama {
    url: String,
    bodies: Arc<Mutex<Vec<Value>>>,
}

/// Answers requests in order. The last reply repeats.
fn fake_ollama(replies: Vec<Reply>) -> FakeOllama {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let seen = bodies.clone();
    std::thread::spawn(move || {
        for (n, stream) in listener.incoming().enumerate() {
            let Ok(stream) = stream else { break };
            let reply = replies.get(n).or(replies.last()).cloned().unwrap();
            serve(stream, reply, &seen);
        }
    });
    FakeOllama { url, bodies }
}

fn serve(mut stream: TcpStream, reply: Reply, seen: &Mutex<Vec<Value>>) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            length = v.trim().parse().unwrap();
        }
    }
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).unwrap();
    seen.lock().unwrap().push(serde_json::from_slice(&body).unwrap());
    let head = |status: &str, len: usize| {
        format!("HTTP/1.1 {status}\r\nContent-Type: application/x-ndjson\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n")
    };
    match reply {
        Reply::Stream(chunks) => {
            let text: String = chunks.iter().map(|c| format!("{c}\n")).collect();
            let _ = stream.write_all(format!("{}{text}", head("200 OK", text.len())).as_bytes());
        }
        Reply::Status(code, text) => {
            let _ = stream.write_all(format!("{}{text}", head(&format!("{code} Error"), text.len())).as_bytes());
        }
        Reply::Hang(chunk) => {
            let _ = stream.write_all(format!("{}{chunk}\n", head("200 OK", 100_000)).as_bytes());
            std::thread::sleep(Duration::from_secs(30));
        }
    }
}

fn ollama_at(url: &str) -> Settings {
    Settings { paths: PathSettings { ollama: Some(url.to_string()), ..Default::default() }, ..Default::default() }
}

fn chunk(content: &str) -> Value {
    json!({ "model": "qwen3:4b", "message": { "role": "assistant", "content": content }, "done": false })
}

fn done(prompt: u64, eval: u64) -> Value {
    json!({ "model": "qwen3:4b", "message": { "role": "assistant", "content": "" }, "done": true, "done_reason": "stop", "prompt_eval_count": prompt, "eval_count": eval })
}

fn tool_calls(calls: Value) -> Value {
    json!({ "model": "qwen3:4b", "message": { "role": "assistant", "content": "", "tool_calls": calls }, "done": false })
}

#[test]
fn ollama_loop_runs_tools_and_keeps_the_session() {
    let dir = scratch("ollama-agent");
    std::fs::write(dir.join("notes.txt"), "hello notes").unwrap();
    let bin = scratch("ollama-mcp");
    let mcp_log = bin.join("calls.log");
    let server = fake_ollama(vec![
        Reply::Stream(vec![
            json!({ "model": "qwen3:4b", "message": { "role": "assistant", "content": "", "thinking": "hmm" }, "done": false }),
            chunk("<think>inline thoughts</think>Let me "),
            chunk("look."),
            tool_calls(json!([
                { "id": "call_1", "function": { "index": 0, "name": "read_file", "arguments": { "path": "notes.txt" } } },
                { "id": "call_2", "function": { "index": 1, "name": "note", "arguments": { "text": "reading notes" } } },
                { "id": "call_3", "function": { "index": 2, "name": "read_file", "arguments": { "path": "/etc/passwd" } } },
            ])),
            done(100, 20),
        ]),
        Reply::Stream(vec![chunk("All "), chunk("done."), done(50, 5)]),
    ]);
    let runtime = create(&config(RuntimeKind::Ollama, Some("qwen3:4b")), &ollama_at(&server.url)).unwrap();
    assert_eq!(runtime.name(), "ollama");
    let mcp = fake_mcp(&bin, &mcp_log);
    let handle = runtime.start(request(&dir, "Read my notes", Permissions { write: true, ..Default::default() }, Some(mcp))).unwrap();
    let events = collect(&handle, LIMIT);

    let id = session(&events).expect("a new session id");
    assert_eq!(
        events,
        vec![
            RunEvent::Session { id: id.clone() },
            RunEvent::Text { text: "Let me look.".into() },
            RunEvent::Tool { tool: "files.read".into(), summary: "notes.txt".into(), target: None },
            RunEvent::Tool { tool: "meerora.note".into(), summary: "reading notes".into(), target: None },
            RunEvent::Tool { tool: "files.read".into(), summary: "/etc/passwd".into(), target: None },
            RunEvent::Text { text: "All done.".into() },
            RunEvent::Usage { input_tokens: 150, output_tokens: 25 },
            RunEvent::Finished { result: Ok("All done.".into()) },
        ]
    );

    let bodies = server.bodies.lock().unwrap().clone();
    assert_eq!(bodies.len(), 2);
    let offered: Vec<&str> = bodies[0]["tools"].as_array().unwrap().iter().map(|t| t["function"]["name"].as_str().unwrap()).collect();
    assert_eq!(offered, vec!["read_file", "list_files", "write_file", "note", "go_to"]);
    assert_eq!(bodies[0]["stream"], true);
    assert_eq!(bodies[0]["messages"][0]["role"], "system");
    let second = bodies[1]["messages"].as_array().unwrap();
    let tool_answers: Vec<&str> = second.iter().filter(|m| m["role"] == "tool").map(|m| m["content"].as_str().unwrap()).collect();
    assert_eq!(tool_answers[0], "hello notes");
    assert_eq!(tool_answers[1], "noted");
    assert!(tool_answers[2].starts_with("error: /etc/passwd is outside the agent directory"), "{}", tool_answers[2]);
    assert_eq!(second.iter().find(|m| m["role"] == "tool").unwrap()["tool_call_id"], "call_1");
    assert_eq!(second.iter().find(|m| m["role"] == "assistant").unwrap()["content"], "Let me look.");

    let calls = std::fs::read_to_string(&mcp_log).unwrap();
    assert!(calls.contains("\"name\":\"note\"") && calls.contains("reading notes"), "{calls}");
    let saved: Value = serde_json::from_str(&std::fs::read_to_string(dir.join(format!("logs/sessions/{id}.json"))).unwrap()).unwrap();
    let messages = saved["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 6, "user, assistant, three tool answers, assistant");
    assert_eq!(messages[0]["content"], "Read my notes");

    // Continue the session: the history goes out before the new prompt.
    let server = fake_ollama(vec![Reply::Stream(vec![chunk("Again."), done(10, 2)])]);
    let runtime = create(&config(RuntimeKind::Ollama, Some("qwen3:4b")), &ollama_at(&server.url)).unwrap();
    let mut req = request(&dir, "And now?", Permissions::default(), None);
    req.session = Some(id.clone());
    let events = collect(&runtime.start(req).unwrap(), LIMIT);
    assert_eq!(events[0], RunEvent::Session { id: id.clone() });
    assert_eq!(result(&events), Ok("Again.".into()));
    let body = server.bodies.lock().unwrap()[0].clone();
    let sent = body["messages"].as_array().unwrap();
    assert_eq!(sent.len(), 1 + 6 + 1);
    assert_eq!(sent[1]["content"], "Read my notes");
    assert_eq!(sent[7]["content"], "And now?");
    let offered: Vec<&str> = body["tools"].as_array().unwrap().iter().map(|t| t["function"]["name"].as_str().unwrap()).collect();
    assert_eq!(offered, vec!["read_file", "list_files"], "a read-only run gets no write_file");
}

#[test]
fn ollama_errors_are_readable() {
    let dir = scratch("ollama-errors");
    let run = |settings: Settings, model: Option<&str>| {
        let runtime = create(&config(RuntimeKind::Ollama, model), &settings).unwrap();
        result(&collect(&runtime.start(request(&dir, "hi", Permissions::default(), None)).unwrap(), LIMIT))
    };

    // Linux refuses every connection to port 0, because no socket can listen
    // there. A port freed by dropping a listener could be taken by the fake
    // server of a parallel test.
    let url = "http://127.0.0.1:0";
    assert_eq!(run(ollama_at(url), Some("qwen3:4b")), Err(format!("ollama is not running at {url}")));

    let server = fake_ollama(vec![Reply::Status(404, r#"{"error":"model \"nope\" not found, try pulling it first"}"#.into())]);
    assert_eq!(run(ollama_at(&server.url), Some("nope")), Err("model nope not found (run: ollama pull nope)".into()));

    let server = fake_ollama(vec![Reply::Status(400, r#"{"error":"tinydolphin does not support tools"}"#.into())]);
    assert_eq!(run(ollama_at(&server.url), Some("tinydolphin")), Err("ollama answered 400: tinydolphin does not support tools".into()));

    assert_eq!(run(Settings::default(), None), Err("no model set for the ollama runtime (set model in [runtime])".into()));
}

#[test]
fn ollama_stops_after_the_turn_limit() {
    let dir = scratch("ollama-limit");
    let server =
        fake_ollama(vec![Reply::Stream(vec![tool_calls(json!([{ "function": { "name": "list_files", "arguments": {} } }])), done(1, 1)])]);
    let runtime = create(&config(RuntimeKind::Ollama, Some("qwen3:4b")), &ollama_at(&server.url)).unwrap();
    let events = collect(&runtime.start(request(&dir, "loop", Permissions::default(), None)).unwrap(), LIMIT);
    assert_eq!(result(&events), Err(format!("qwen3:4b was still calling tools after {} turns", meerora_runtime::OLLAMA_MAX_TURNS)));
    assert_eq!(server.bodies.lock().unwrap().len(), meerora_runtime::OLLAMA_MAX_TURNS);
    assert_eq!(tools(&events).len(), meerora_runtime::OLLAMA_MAX_TURNS);
    assert_eq!(events[events.len() - 2], RunEvent::Usage { input_tokens: 12, output_tokens: 12 });
}

#[test]
fn ollama_cancel_finishes_at_once() {
    let dir = scratch("ollama-cancel");
    let server = fake_ollama(vec![Reply::Hang(chunk("thinking about it"))]);
    let runtime = create(&config(RuntimeKind::Ollama, Some("qwen3:4b")), &ollama_at(&server.url)).unwrap();
    let handle = runtime.start(request(&dir, "hi", Permissions::default(), None)).unwrap();
    assert!(matches!(handle.events.recv_timeout(LIMIT).unwrap(), RunEvent::Session { .. }));
    let deadline = Instant::now() + LIMIT;
    while server.bodies.lock().unwrap().is_empty() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
    let started = Instant::now();
    handle.cancel();
    let events = collect(&handle, LIMIT);
    assert_eq!(events, vec![RunEvent::Finished { result: Err("cancelled".into()) }]);
    assert!(started.elapsed() < Duration::from_secs(1));
}
