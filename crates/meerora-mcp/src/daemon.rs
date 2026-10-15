//! The connection to the agent daemon. One Unix socket connection is reused
//! for every call and opened again when it drops. A reader thread drains all
//! events, because the daemon broadcasts everything to every client and drops
//! clients that stop reading. Only hellos and tool results are passed on.

use std::io::{BufRead, BufReader, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use meerora_protocol::{Command, Event, PROTOCOL_VERSION, ToolCall, decode_event, encode};

/// How long a write to a daemon that stopped reading may block. The daemon
/// uses the same limit for its clients.
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);
/// How long `request_capability` waits for the user's decision (D8). The
/// runtimes wait a minute longer (`meerora_runtime::DECISION_WAIT`), so this
/// bridge always answers first.
pub const DECISION_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// How long the app tools of D30 wait for the app. The daemon gives up after 30
/// seconds (meerora-agentd app_tools.rs), so its answer always arrives first.
pub const APP_TIMEOUT: Duration = Duration::from_secs(35);

/// The daemon's answer to a tool call.
#[derive(Clone, Debug, PartialEq)]
pub struct Reply {
    pub ok: bool,
    pub message: String,
    /// A PNG as base64, from `app_screenshot` (D30).
    pub image: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum CallError {
    /// No daemon answers on the socket. The call did nothing.
    Unavailable(String),
    /// No answer within the timeout. The call may or may not have happened.
    Timeout(Duration),
    /// The connection closed after the call was sent.
    Dropped,
}

impl CallError {
    /// Text for the model, `tool` being the MCP tool name.
    pub fn describe(&self, tool: &str) -> String {
        match self {
            CallError::Unavailable(detail) => format!("meerora is not running, so {tool} had no effect. {detail}"),
            CallError::Timeout(t) => {
                format!("meerora did not answer within {} seconds, so it is unclear whether {tool} took effect.", t.as_secs_f64())
            }
            CallError::Dropped => {
                format!("The connection to meerora closed before it answered, so it is unclear whether {tool} took effect.")
            }
        }
    }
}

enum Incoming {
    Hello { version: u32, daemon: String },
    Result { id: String, ok: bool, message: String, image: Option<String> },
}

struct Connection {
    stream: UnixStream,
    incoming: Receiver<Incoming>,
}

impl Drop for Connection {
    /// Also ends the reader thread, which is blocked on the other half.
    fn drop(&mut self) {
        let _ = self.stream.shutdown(Shutdown::Both);
    }
}

pub struct Daemon {
    socket: PathBuf,
    agent: String,
    timeout: Duration,
    /// The timeout of `request_capability`, which waits for the user.
    decision_timeout: Duration,
    /// The timeout of the app tools, which wait for the app (D30).
    app_timeout: Duration,
    conn: Option<Connection>,
    /// Prefix of the call ids. Results may reach every client of the daemon,
    /// so ids have to be unique across bridges.
    id_prefix: String,
    next_id: u64,
}

impl Daemon {
    /// Connects lazily, on the first call.
    pub fn new(socket: PathBuf, agent: &str, timeout: Duration) -> Self {
        let id_prefix = format!("mcp-{agent}-{}", std::process::id());
        Self {
            socket,
            agent: agent.to_string(),
            timeout,
            decision_timeout: DECISION_TIMEOUT,
            app_timeout: APP_TIMEOUT,
            conn: None,
            id_prefix,
            next_id: 0,
        }
    }

    #[cfg(test)]
    pub fn with_decision_timeout(mut self, timeout: Duration) -> Self {
        self.decision_timeout = timeout;
        self
    }

    #[cfg(test)]
    pub fn with_app_timeout(mut self, timeout: Duration) -> Self {
        self.app_timeout = timeout;
        self
    }

    /// Sends `Command::Tool` and waits for the `Event::ToolResult` with the
    /// same id. Connecting counts towards the timeout. `request_capability`
    /// waits for the user, so it gets the long [`DECISION_TIMEOUT`], and the app
    /// tools wait for the app with [`APP_TIMEOUT`].
    pub fn call(&mut self, call: ToolCall) -> Result<Reply, CallError> {
        let timeout = match call {
            // config_change waits for the user like request_capability.
            ToolCall::RequestCapability { .. } | ToolCall::ConfigChange { .. } => self.decision_timeout,
            // config_read check runs meerora-agentd check, which takes a moment.
            ToolCall::ConfigRead { .. } => self.app_timeout,
            ToolCall::AppsList
            | ToolCall::AppRead { .. }
            | ToolCall::AppScreenshot { .. }
            | ToolCall::AppAct { .. }
            | ToolCall::AppOpen { .. } => self.app_timeout,
            _ => self.timeout,
        };
        let deadline = Instant::now() + timeout;
        self.next_id += 1;
        let id = format!("{}-{}", self.id_prefix, self.next_id);
        let line = encode(&Command::Tool { id: id.clone(), agent: self.agent.clone(), call });

        self.forget_closed_connection();
        let mut attempts = 0;
        let conn = loop {
            let mut conn = match self.conn.take() {
                Some(conn) => conn,
                None => self.connect(deadline)?,
            };
            match conn.stream.write_all(line.as_bytes()) {
                Ok(()) => break self.conn.insert(conn),
                // A reused connection may have died since the last call.
                Err(e) if attempts == 0 => crate::log!("the connection to the daemon failed ({e}), connecting again"),
                Err(e) => return Err(CallError::Unavailable(format!("Cannot send to {}. {e}.", self.socket.display()))),
            }
            attempts += 1;
        };

        loop {
            match conn.incoming.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(Incoming::Result { id: got, ok, message, image }) if got == id => return Ok(Reply { ok, message, image }),
                Ok(_) => continue,
                Err(RecvTimeoutError::Timeout) => return Err(CallError::Timeout(timeout)),
                Err(RecvTimeoutError::Disconnected) => {
                    self.conn = None;
                    return Err(CallError::Dropped);
                }
            }
        }
    }

    /// Drops results of earlier calls that timed out, and the connection if
    /// the daemon closed it since the last call.
    fn forget_closed_connection(&mut self) {
        let Some(conn) = &self.conn else { return };
        loop {
            match conn.incoming.try_recv() {
                Ok(_) => continue,
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => break,
            }
        }
        crate::log!("the daemon closed the connection, connecting again");
        self.conn = None;
    }

    fn connect(&self, deadline: Instant) -> Result<Connection, CallError> {
        let socket = self.socket.display();
        let unavailable = |what: &str, e: std::io::Error| CallError::Unavailable(format!("{what} {socket}. {e}."));
        let stream = UnixStream::connect(&self.socket).map_err(|e| unavailable("Cannot connect to", e))?;
        let reader = stream.try_clone().map_err(|e| unavailable("Cannot read from", e))?;
        let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
        let (tx, incoming) = mpsc::channel();
        thread::Builder::new()
            .name("daemon-events".into())
            .spawn(move || read_events(reader, &tx))
            .map_err(|e| unavailable("Cannot start a reader for", e))?;
        let mut conn = Connection { stream, incoming };

        let hello =
            Command::Hello { version: PROTOCOL_VERSION, client: format!("meerora-mcp {} ({})", env!("CARGO_PKG_VERSION"), self.agent) };
        conn.stream.write_all(encode(&hello).as_bytes()).map_err(|e| unavailable("Cannot send to", e))?;
        loop {
            match conn.incoming.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(Incoming::Hello { version, daemon }) => {
                    if version != PROTOCOL_VERSION {
                        crate::log!("{daemon} speaks protocol version {version}, this bridge speaks {PROTOCOL_VERSION}");
                    }
                    crate::log!("connected to {daemon} at {socket}");
                    return Ok(conn);
                }
                Ok(Incoming::Result { .. }) => continue,
                // The command was not sent yet, so the call surely did nothing.
                Err(RecvTimeoutError::Timeout) => {
                    return Err(CallError::Unavailable(format!(
                        "{socket} did not answer the hello within {} seconds.",
                        self.timeout.as_secs_f64()
                    )));
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(CallError::Unavailable(format!("{socket} closed the connection without a hello.")));
                }
            }
        }
    }
}

fn read_events(stream: UnixStream, tx: &Sender<Incoming>) {
    let mut reader = BufReader::new(stream);
    let mut line = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        // Events this bridge does not know, for example from a newer
        // daemon, fail to decode and are skipped like all other events.
        let incoming = match decode_event(&String::from_utf8_lossy(&line)) {
            Ok(Event::ToolResult { id, ok, message, image }) => Incoming::Result { id, ok, message, image },
            Ok(Event::Hello { version, daemon }) => Incoming::Hello { version, daemon },
            _ => continue,
        };
        if tx.send(incoming).is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{FakeDaemon, Reaction, TestDir, answer};

    const SHORT: Duration = Duration::from_millis(400);

    fn note(text: &str) -> ToolCall {
        ToolCall::Note { text: text.into() }
    }

    #[test]
    fn round_trip_skips_other_events() {
        let dir = TestDir::new("daemon-ok");
        let fake = FakeDaemon::start(&dir.path("d.sock"), answer(true, "Walking to the desk."));
        let mut daemon = Daemon::new(fake.socket.clone(), "wren", Duration::from_secs(5));

        let reply = daemon.call(ToolCall::GoTo { station: "desk".into() }).unwrap();
        assert_eq!(reply, Reply { ok: true, message: "Walking to the desk.".into(), image: None });
        match fake.commands.recv_timeout(SHORT).unwrap() {
            Command::Hello { version, client } => {
                assert_eq!(version, PROTOCOL_VERSION);
                assert!(client.starts_with("meerora-mcp "), "{client}");
            }
            other => panic!("expected hello first, got {other:?}"),
        }
        match fake.commands.recv_timeout(SHORT).unwrap() {
            Command::Tool { id, agent, call } => {
                assert!(id.starts_with("mcp-wren-"), "{id}");
                assert_eq!(agent, "wren");
                assert_eq!(call, ToolCall::GoTo { station: "desk".into() });
            }
            other => panic!("expected a tool call, got {other:?}"),
        }

        // The second call reuses the connection, so there is no second hello.
        assert!(daemon.call(note("hi")).unwrap().ok);
        assert!(matches!(fake.commands.recv_timeout(SHORT).unwrap(), Command::Tool { .. }));
    }

    #[test]
    fn refusals_are_replies() {
        let dir = TestDir::new("daemon-refuse");
        let fake = FakeDaemon::start(&dir.path("d.sock"), answer(false, "letter is rate limited"));
        let mut daemon = Daemon::new(fake.socket.clone(), "wren", Duration::from_secs(5));
        let reply = daemon.call(ToolCall::FigureSet { name: "letter".into(), value: serde_json::json!(true) }).unwrap();
        assert_eq!(reply, Reply { ok: false, message: "letter is rate limited".into(), image: None });
    }

    #[test]
    fn a_missing_daemon_is_unavailable() {
        let dir = TestDir::new("daemon-none");
        let mut daemon = Daemon::new(dir.path("nothing.sock"), "wren", SHORT);
        let err = daemon.call(note("hi")).unwrap_err();
        let CallError::Unavailable(detail) = &err else { panic!("{err:?}") };
        assert!(detail.contains("nothing.sock"), "{detail}");
        assert!(err.describe("note").starts_with("meerora is not running, so note had no effect."));
    }

    #[test]
    fn a_socket_without_hello_is_unavailable() {
        let dir = TestDir::new("daemon-mute");
        let socket = dir.path("d.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        // Accepts and keeps the connection open, but never writes.
        thread::spawn(move || {
            let _held: Vec<UnixStream> = listener.incoming().map_while(Result::ok).collect();
        });
        let mut daemon = Daemon::new(socket, "wren", SHORT);
        let err = daemon.call(note("hi")).unwrap_err();
        assert!(
            matches!(&err, CallError::Unavailable(detail) if detail.contains("did not answer the hello within 0.4 seconds")),
            "{err:?}"
        );
    }

    #[test]
    fn a_silent_daemon_times_out() {
        let dir = TestDir::new("daemon-silent");
        let fake = FakeDaemon::start(&dir.path("d.sock"), |_: &Command| Reaction::Ignore);
        let mut daemon = Daemon::new(fake.socket.clone(), "wren", SHORT);
        let started = Instant::now();
        assert_eq!(daemon.call(note("hi")), Err(CallError::Timeout(SHORT)));
        assert!(started.elapsed() < SHORT * 3);
        assert!(CallError::Timeout(Duration::from_secs(5)).describe("note").contains("within 5 seconds"));
    }

    #[test]
    fn capability_requests_wait_longer() {
        let dir = TestDir::new("daemon-decide");
        // Answers notes at once and capability requests after 600 ms, like a user who thinks.
        let fake = FakeDaemon::start(&dir.path("d.sock"), |command: &Command| match command {
            Command::Tool { id, call: ToolCall::RequestCapability { .. }, .. } => {
                thread::sleep(Duration::from_millis(600));
                Reaction::Reply(vec![Event::ToolResult { id: id.clone(), ok: true, message: "The user allowed web.".into(), image: None }])
            }
            other => answer(true, "")(other),
        });
        let mut daemon = Daemon::new(fake.socket.clone(), "wren", SHORT).with_decision_timeout(Duration::from_secs(5));
        let request = ToolCall::RequestCapability { capability: "web".into(), reason: "docs".into() };
        assert_eq!(daemon.call(request.clone()).unwrap().message, "The user allowed web.", "longer than the short timeout");
        let mut daemon = Daemon::new(fake.socket.clone(), "wren", SHORT).with_decision_timeout(Duration::from_millis(200));
        assert_eq!(daemon.call(request), Err(CallError::Timeout(Duration::from_millis(200))));
        assert_eq!(DECISION_TIMEOUT, Duration::from_secs(600));
    }

    #[test]
    fn app_tools_wait_for_the_app_and_bring_images() {
        let dir = TestDir::new("daemon-app");
        // Answers screenshots after 600 ms, like an app that renders first.
        let fake = FakeDaemon::start(&dir.path("d.sock"), |command: &Command| match command {
            Command::Tool { id, call: ToolCall::AppScreenshot { .. }, .. } => {
                thread::sleep(Duration::from_millis(600));
                let shot = Event::ToolResult { id: id.clone(), ok: true, message: "A picture.".into(), image: Some("iVBO".into()) };
                Reaction::Reply(vec![shot])
            }
            other => answer(true, "")(other),
        });
        let mut daemon = Daemon::new(fake.socket.clone(), "wren", SHORT).with_app_timeout(Duration::from_secs(5));
        let reply = daemon.call(ToolCall::AppScreenshot { app: "browser:1".into() }).unwrap();
        assert_eq!(reply, Reply { ok: true, message: "A picture.".into(), image: Some("iVBO".into()) }, "longer than the short timeout");
        let mut daemon = Daemon::new(fake.socket.clone(), "wren", SHORT).with_app_timeout(Duration::from_millis(200));
        assert_eq!(daemon.call(ToolCall::AppScreenshot { app: "browser:1".into() }), Err(CallError::Timeout(Duration::from_millis(200))));
        assert!(APP_TIMEOUT > Duration::from_secs(30), "the daemon's own timeout of 30 s answers first");
    }

    #[test]
    fn a_dropped_connection_is_opened_again() {
        let dir = TestDir::new("daemon-drop");
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let ok = answer(true, "");
        let fake = FakeDaemon::start(&dir.path("d.sock"), move |command: &Command| {
            if calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 { Reaction::Close } else { ok(command) }
        });
        let mut daemon = Daemon::new(fake.socket.clone(), "wren", Duration::from_secs(5));
        assert_eq!(daemon.call(note("first")), Err(CallError::Dropped));
        assert_eq!(daemon.call(note("second")), Ok(Reply { ok: true, message: String::new(), image: None }));
        let hellos = std::iter::from_fn(|| fake.commands.recv_timeout(SHORT).ok()).filter(|c| matches!(c, Command::Hello { .. })).count();
        assert_eq!(hellos, 2);
    }

    #[test]
    fn a_connection_closed_between_calls_is_opened_again() {
        let dir = TestDir::new("daemon-restart");
        let ok = answer(true, "done");
        let fake = FakeDaemon::start(&dir.path("d.sock"), move |command: &Command| match ok(command) {
            Reaction::Reply(events) => Reaction::ReplyThenClose(events),
            other => other,
        });
        let mut daemon = Daemon::new(fake.socket.clone(), "wren", Duration::from_secs(5));
        assert_eq!(daemon.call(note("a")).unwrap().message, "done");
        thread::sleep(Duration::from_millis(100));
        assert_eq!(daemon.call(note("b")).unwrap().message, "done");
        let hellos = std::iter::from_fn(|| fake.commands.recv_timeout(SHORT).ok()).filter(|c| matches!(c, Command::Hello { .. })).count();
        assert_eq!(hellos, 2);
    }
}
