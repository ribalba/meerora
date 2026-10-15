//! Helpers for the unit tests and for `tests/stdio.rs`, which includes this
//! file: temporary data dirs and a fake daemon on a Unix socket.

#![allow(dead_code)]

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;

use meerora_protocol::{Command, Event, PROTOCOL_VERSION, RingState, decode_command, encode};

/// A directory below the system temp dir that is removed again at the end.
/// The path stays short because socket paths are limited to about 100 bytes.
pub struct TestDir(pub PathBuf);

impl TestDir {
    pub fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("meerora-mcp-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    pub fn path(&self, rel: &str) -> PathBuf {
        self.0.join(rel)
    }

    pub fn write(&self, rel: &str, text: &str) {
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

/// A small figure with one toggle param, `letter`.
pub const FIGURE: &str = r##"
const body = box(20, 30, 20).material("felt", "#C8932F");
const hat = cone(12, 10).material("felt", "#3A3A3A").on(body);
const letter = box(12, 8, 1).material("paper", "#F4F1EA").beside(body, "right");
export default figure({
  parts: [body, hat],
  anchors: { ring: hat.top.above(10), face: body },
  animations: { working: bob(), waiting: hop(2), resting: breathe() },
  params: { letter: toggle(false).shows(letter) },
});
"##;

/// A small space with two connected stations, `desk` and `door`.
pub const SPACE: &str = r##"
const floor = box(400, 2, 400).material("wood", "#8B6B4A");
export default space({
  parts: [floor],
  stations: { desk: station([100, 0, 100], { facing: 0 }), door: [300, 0, 300] },
  paths: [["desk", "door"]],
});
"##;

/// Writes the agent `wren` with [`FIGURE`] and the space [`SPACE`] below `dir`.
pub fn write_data_dir(dir: &TestDir) {
    dir.write("agents/wren/agent.toml", "name = \"Wren\"\nhome = \"desk\"\n");
    dir.write("agents/wren/figure.js", FIGURE);
    dir.write("space/space.js", SPACE);
}

/// What the fake daemon does with a command other than hello.
pub enum Reaction {
    Reply(Vec<Event>),
    /// Answers, then closes the connection like a daemon that stops.
    ReplyThenClose(Vec<Event>),
    Ignore,
    /// Closes the connection without answering.
    Close,
}

/// Answers every `Command::Tool` with a `ToolResult` for its id, after two
/// events the bridge has to skip: a ring change and a result for another id.
pub fn answer(ok: bool, message: &str) -> impl Fn(&Command) -> Reaction + Send + Sync + 'static {
    let message = message.to_string();
    move |command| match command {
        Command::Tool { id, .. } => Reaction::Reply(vec![
            Event::Ring { agent: "ember".into(), state: RingState::Working, progress: None },
            Event::ToolResult { id: format!("other-{id}"), ok: !ok, message: "not for you".into(), image: None },
            Event::ToolResult { id: id.clone(), ok, message: message.clone(), image: None },
        ]),
        _ => Reaction::Ignore,
    }
}

/// A daemon that greets like the real one and reacts to commands with a
/// closure. Every command it receives arrives on [`FakeDaemon::commands`].
pub struct FakeDaemon {
    pub socket: PathBuf,
    pub commands: Receiver<Command>,
}

impl FakeDaemon {
    pub fn start(socket: &Path, react: impl Fn(&Command) -> Reaction + Send + Sync + 'static) -> Self {
        let _ = fs::remove_file(socket);
        let listener = UnixListener::bind(socket).unwrap();
        let (tx, rx) = mpsc::channel();
        let react = Arc::new(react);
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { return };
                let (tx, react) = (tx.clone(), react.clone());
                thread::spawn(move || serve(stream, &tx, &*react));
            }
        });
        Self { socket: socket.to_path_buf(), commands: rx }
    }

    /// The next command other than hello, or None after `timeout`.
    pub fn next_command(&self, timeout: Duration) -> Option<Command> {
        loop {
            match self.commands.recv_timeout(timeout).ok()? {
                Command::Hello { .. } => continue,
                other => return Some(other),
            }
        }
    }
}

fn serve(stream: UnixStream, tx: &Sender<Command>, react: &dyn Fn(&Command) -> Reaction) {
    let mut writer = stream.try_clone().unwrap();
    for line in BufReader::new(stream).lines() {
        let Ok(line) = line else { return };
        let Ok(command) = decode_command(&line) else { continue };
        let _ = tx.send(command.clone());
        let (events, close) = match &command {
            Command::Hello { .. } => (
                vec![
                    Event::Hello { version: PROTOCOL_VERSION, daemon: "fake daemon".into() },
                    Event::AgentSpawned { agent: "wren".into(), name: "Wren".into(), system: false },
                ],
                false,
            ),
            other => match react(other) {
                Reaction::Reply(events) => (events, false),
                Reaction::ReplyThenClose(events) => (events, true),
                Reaction::Ignore => continue,
                Reaction::Close => return,
            },
        };
        let payload: String = events.iter().map(encode).collect();
        if writer.write_all(payload.as_bytes()).is_err() || close {
            return;
        }
    }
}
