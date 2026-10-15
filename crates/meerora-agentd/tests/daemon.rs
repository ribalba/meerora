//! Starts the real daemon on a temporary socket and talks to it like the app.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command as Process, ExitStatus, Output, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use meerora_protocol::{Command, Event, PROTOCOL_VERSION, RingState, Role, decode_event, encode};
use serde_json::json;

const BIN: &str = env!("CARGO_BIN_EXE_meerora-agentd");
const TIMEOUT: Duration = Duration::from_secs(10);

/// A directory below the system temp dir that is removed again at the end.
/// The path stays short because socket paths are limited to about 100 bytes.
struct TestDir(PathBuf);

impl TestDir {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("meerora-agentd-{name}-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.0.join(rel)
    }

    fn arg(&self, rel: &str) -> String {
        self.path(rel).to_str().unwrap().to_string()
    }

    fn write(&self, rel: &str, text: &str) {
        let path = self.path(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, text).unwrap();
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Kills the daemon if the test fails before it stops it.
struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn wait_for_exit(child: &mut Child) -> ExitStatus {
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        assert!(start.elapsed() < TIMEOUT, "the process did not exit");
        sleep(Duration::from_millis(20));
    }
}

const SCENARIO: &str = r#"
loop_seconds = 600

[[step]]
at = 0
agent = "alpha"
ring = "working"
progress = 0.5
go_to = "workbench"
activity = "testing"
param = { lamp = true }

[[step]]
at = 500
agent = "alpha"
activity = "later"

[[runs.beta.step]]
go_to = "mailbox"
"#;

fn data_dir(name: &str, scenario: &str) -> TestDir {
    let dir = TestDir::new(name);
    // Alpha has the helper role, so the daemon creates no helper of its own (helper.rs).
    dir.write("agents/alpha/agent.toml", "name = \"Alpha\"\nhome = \"desk\"\nrole = \"helper\"\n[capabilities]\nsystem = true\n");
    dir.write("agents/beta/agent.toml", "name = \"Beta\"\nhome = \"window\"\n[[schedule]]\nevery = \"1h\"\ntask = \"Check mail.\"\n");
    dir.write("scenario.toml", scenario);
    dir
}

fn spawn_daemon(dir: &TestDir, log: &str) -> Child {
    Process::new(BIN)
        .args(["--data-dir", &dir.arg(""), "--scenario", &dir.arg("scenario.toml"), "--socket", &dir.arg("run/agentd.sock")])
        .env("RUST_LOG", "debug")
        .stdout(Stdio::null())
        .stderr(fs::File::create(dir.path(log)).unwrap())
        .spawn()
        .unwrap()
}

fn connect(socket: &Path) -> UnixStream {
    let start = Instant::now();
    loop {
        if let Ok(stream) = UnixStream::connect(socket) {
            stream.set_read_timeout(Some(TIMEOUT)).unwrap();
            return stream;
        }
        assert!(start.elapsed() < TIMEOUT, "the daemon did not open {}", socket.display());
        sleep(Duration::from_millis(20));
    }
}

fn next_event(reader: &mut BufReader<UnixStream>) -> Event {
    let mut line = String::new();
    reader.read_line(&mut line).expect("an event before the timeout");
    decode_event(&line).unwrap_or_else(|e| panic!("{e}: {line:?}"))
}

fn run_tool(args: &[&str]) -> Output {
    Process::new(BIN).args(args).env("RUST_LOG", "warn").output().unwrap()
}

#[test]
fn serves_snapshot_injects_acks_and_cleans_up() {
    let dir = data_dir("serve", SCENARIO);
    let socket = dir.path("run/agentd.sock");
    let socket_arg = dir.arg("run/agentd.sock");
    let mut daemon = Daemon(spawn_daemon(&dir, "daemon.log"));

    let mut stream = connect(&socket);
    let mode = fs::metadata(dir.path("run")).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o700, "the socket directory is private");

    // Hello, then the full state as ordinary events.
    stream.write_all(encode(&Command::Hello { version: PROTOCOL_VERSION, client: "test".into() }).as_bytes()).unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let Event::Hello { version, daemon: name } = next_event(&mut reader) else { panic!("expected hello first") };
    assert_eq!(version, PROTOCOL_VERSION);
    assert!(name.starts_with("meerora-agentd"), "{name}");
    let s = |v: &str| v.to_string();
    let expected = [
        Event::AgentSpawned { agent: s("alpha"), name: s("Alpha"), system: true },
        Event::AgentSpawned { agent: s("beta"), name: s("Beta"), system: false },
        Event::Ring { agent: s("alpha"), state: RingState::Working, progress: Some(0.5) },
        Event::GoTo { agent: s("alpha"), station: s("workbench") },
        Event::Activity { agent: s("alpha"), text: s("testing") },
        Event::Param { agent: s("alpha"), name: s("lamp"), value: json!(true) },
        Event::Ring { agent: s("beta"), state: RingState::Resting, progress: None },
        Event::GoTo { agent: s("beta"), station: s("window") },
        Event::Activity { agent: s("beta"), text: s("") },
    ];
    let snapshot: Vec<Event> = expected.iter().map(|_| next_event(&mut reader)).collect();
    assert_eq!(snapshot, expected);
    // Then the social state (D26 to D28): no account, no profile, no drafts, no digest.
    assert!(matches!(next_event(&mut reader), Event::Account { handle: None, .. }));
    assert!(matches!(next_event(&mut reader), Event::OwnProfile { profile: None, .. }));
    assert!(matches!(next_event(&mut reader), Event::PostDrafts { drafts, .. } if drafts.is_empty()));
    assert!(matches!(next_event(&mut reader), Event::NewsDigest { digest: None }));
    // And the graveyard, empty.
    assert!(matches!(next_event(&mut reader), Event::Graveyard { agents } if agents.is_empty()));

    // A second daemon on the same socket refuses to start.
    let mut second = spawn_daemon(&dir, "second.log");
    assert!(!wait_for_exit(&mut second).success());
    let second_log = fs::read_to_string(dir.path("second.log")).unwrap();
    assert!(second_log.contains("already running"), "{second_log}");

    // The client's own inject comes back to it.
    let activity = Event::Activity { agent: s("alpha"), text: s("injected") };
    stream.write_all(encode(&Command::Inject { event: activity.clone() }).as_bytes()).unwrap();
    assert_eq!(next_event(&mut reader), activity);

    // `send` injects for everyone, `ack` rests the waiting agent at home.
    let out = run_tool(&["send", "--socket", &socket_arg, r#"{"type":"ring","agent":"beta","state":"needs_you"}"#]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(next_event(&mut reader), Event::Ring { agent: s("beta"), state: RingState::NeedsYou, progress: None });
    let out = run_tool(&["ack", "--socket", &socket_arg, "beta"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(next_event(&mut reader), Event::Ring { agent: s("beta"), state: RingState::Resting, progress: None });
    assert_eq!(next_event(&mut reader), Event::GoTo { agent: s("beta"), station: s("window") });

    // `say` to a mock agent prints the echo and meerora's note, and every client sees both.
    let out = run_tool(&["say", "--socket", &socket_arg, "alpha", "hello"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let printed: Vec<Event> = String::from_utf8_lossy(&out.stdout).lines().map(|l| decode_event(l).unwrap()).collect();
    assert!(matches!(&printed[..], [Event::Message { role: Role::User, .. }, Event::Message { role: Role::System, .. }]), "{printed:?}");
    assert!(matches!(next_event(&mut reader), Event::Message { role: Role::User, text, .. } if text == "hello"));
    assert!(matches!(next_event(&mut reader), Event::Message { role: Role::System, text, .. } if text.contains("mock agent")));

    // `reload` picks up a new agent directory.
    dir.write("agents/gamma/agent.toml", "name = \"Gamma\"\nhome = \"tree\"\n");
    let out = run_tool(&["reload", "--socket", &socket_arg]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(next_event(&mut reader), Event::AgentSpawned { agent: s("gamma"), name: s("Gamma"), system: false });

    // SIGTERM removes the socket.
    let status = Process::new("kill").args(["-TERM", &daemon.0.id().to_string()]).status().unwrap();
    assert!(status.success());
    assert!(wait_for_exit(&mut daemon.0).success());
    assert!(!socket.exists(), "the socket is gone after SIGTERM");
}

#[test]
fn retire_list_and_restore_from_the_command_line() {
    let dir = data_dir("grave", SCENARIO);
    let socket = dir.path("run/agentd.sock");
    let socket_arg = dir.arg("run/agentd.sock");
    let _daemon = Daemon(spawn_daemon(&dir, "daemon.log"));
    connect(&socket);
    let tool = |args: &[&str]| {
        let out = run_tool(args);
        assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).to_string()
    };
    assert_eq!(tool(&["graveyard", "--socket", &socket_arg]), "The graveyard is empty.\n");
    let retired = tool(&["retire", "--socket", &socket_arg, "beta"]);
    assert!(retired.contains("graveyard/beta") && retired.contains("meerora-agentd restore beta"), "{retired}");
    assert!(dir.path("graveyard/beta/agent.toml").is_file() && !dir.path("agents/beta").exists());
    let listed = tool(&["graveyard", "--socket", &socket_arg]);
    assert!(listed.starts_with("beta  Beta · retired ") && listed.contains(" · 0 runs · "), "{listed}");
    let back = tool(&["restore", "--socket", &socket_arg, "beta"]);
    assert_eq!(back, "beta is back in the space as agents/beta\n");
    assert!(dir.path("agents/beta/agent.toml").is_file() && !dir.path("agents/beta/retired.json").exists());
    let out = run_tool(&["restore", "--socket", &socket_arg, "beta"]);
    assert!(!out.status.success() && String::from_utf8_lossy(&out.stderr).contains("no beta in the graveyard"));
}

#[test]
fn stale_socket_is_replaced() {
    let dir = data_dir("stale", SCENARIO);
    let socket = dir.path("run/agentd.sock");
    fs::create_dir_all(dir.path("run")).unwrap();
    drop(std::os::unix::net::UnixListener::bind(&socket).unwrap());
    assert!(socket.exists());
    let _daemon = Daemon(spawn_daemon(&dir, "daemon.log"));
    let mut stream = connect(&socket);
    stream.write_all(encode(&Command::Hello { version: PROTOCOL_VERSION, client: "test".into() }).as_bytes()).unwrap();
    let mut reader = BufReader::new(stream);
    assert!(matches!(next_event(&mut reader), Event::Hello { .. }));
}

#[test]
fn send_without_daemon_fails() {
    let dir = TestDir::new("nodaemon");
    let out = run_tool(&["send", "--socket", &dir.arg("none.sock"), r#"{"type":"agent_removed","agent":"x"}"#]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("Is meerora-agentd running?"));
}

#[test]
fn check_accepts_the_demo() {
    let assets = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets");
    let out = run_tool(&["check", "--data-dir", assets.to_str().unwrap()]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{stdout}\n{}", String::from_utf8_lossy(&out.stderr));
    assert!(stdout.contains("runs.wren"), "{stdout}");
    assert!(stdout.trim_end().ends_with("ok"), "{stdout}");
}

#[test]
fn check_rejects_an_unknown_station() {
    let dir = data_dir("check", &SCENARIO.replace("go_to = \"mailbox\"", "go_to = \"kitchen\""));
    let out = run_tool(&["check", "--data-dir", &dir.arg(""), "--scenario", &dir.arg("scenario.toml")]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("runs.beta.step #1 (at 0 s) goes to unknown station \"kitchen\""), "{stderr}");

    // The daemon refuses the same scenario.
    let out = Process::new(BIN)
        .args(["--data-dir", &dir.arg(""), "--scenario", &dir.arg("scenario.toml"), "--socket", &dir.arg("x.sock")])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(!dir.path("x.sock").exists());
}
