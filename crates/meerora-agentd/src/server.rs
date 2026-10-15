//! The Unix socket server, the daemon loop and the small clients behind
//! `meerora-agentd send`, `ack`, `say`, `cancel` and `reload`.
//!
//! Every client gets one thread that reads its commands. The daemon runs on
//! the calling thread, owns all state and writes events to the clients, so
//! every client sees the same order of events.
//!
//! The MCP bridge of each agent gets a socket of its own next to the main
//! one (`bridges/<agent>.sock`, see [`bridge_listener`]). Its clients may only
//! say hello and call tools as that agent, and they receive only the answers
//! to their own calls. The sandbox shows an agent only this socket (D16), so
//! an agent cannot decide its own capability requests (D8), speak for other
//! agents or read their conversations.

use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Write};
use std::net::Shutdown;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use meerora_protocol::{Command, Event, PROTOCOL_VERSION, Role, decode_command, decode_event, encode};
use tracing::{debug, info, warn};

use crate::daemon::{Daemon, Msg, Outgoing};

/// Name and version sent in `Event::Hello`.
pub const DAEMON_NAME: &str = concat!("meerora-agentd ", env!("CARGO_PKG_VERSION"));
/// A client that does not read for this long is dropped.
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);
/// How long the command line clients wait for the daemon's hello.
const HELLO_TIMEOUT: Duration = Duration::from_secs(5);
/// How long `say` waits for the run to finish.
const SAY_TIMEOUT: Duration = Duration::from_secs(600);
/// The mock scenario sleeps at most this long, even when nothing is queued for longer.
const MAX_SLEEP: f64 = 3600.0;

/// Maps real time to scenario time, `speed` scenario seconds per second.
#[derive(Clone, Copy, Debug)]
pub struct Clock {
    start: Instant,
    speed: f64,
}

impl Clock {
    pub fn new(speed: f64) -> Self {
        Self { start: Instant::now(), speed }
    }

    /// Scenario seconds since the start.
    pub fn now(&self) -> f64 {
        self.start.elapsed().as_secs_f64() * self.speed
    }

    /// Real time until scenario time `t`.
    pub fn until(&self, t: f64) -> Duration {
        let wait = t / self.speed - self.start.elapsed().as_secs_f64();
        Duration::from_secs_f64(wait.clamp(0.0, MAX_SLEEP))
    }
}

/// Creates the socket and its parent directory (mode 0700). A leftover socket
/// file is removed, unless a daemon still answers on it.
pub fn bind(path: &Path) -> anyhow::Result<UnixListener> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    match std::fs::symlink_metadata(path) {
        Ok(meta) => {
            if !meta.file_type().is_socket() {
                bail!("{} exists and is not a socket, so it is left alone", path.display());
            }
            if UnixStream::connect(path).is_ok() {
                bail!("another meerora-agentd is already running on {}", path.display());
            }
            info!("removing stale socket {}", path.display());
            std::fs::remove_file(path).with_context(|| format!("cannot remove stale socket {}", path.display()))?;
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("cannot inspect {}", path.display())),
    }
    UnixListener::bind(path).with_context(|| format!("cannot listen on {}", path.display()))
}

/// Removes the socket file when dropped, including when the daemon panics.
pub struct SocketGuard(pub PathBuf);

impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Accepts clients on a background thread and runs the daemon on this one.
/// `tx` and `rx` are the daemon's channel. Returns only if accepting stops for good.
pub fn serve(listener: UnixListener, daemon: Daemon, tx: mpsc::Sender<Msg>, rx: mpsc::Receiver<Msg>) -> anyhow::Result<()> {
    thread::Builder::new()
        .name("accept".into())
        .spawn(move || accept_loop(listener, tx, None))
        .context("cannot start the accept thread")?;
    daemon_loop(daemon, rx);
    bail!("the accept thread stopped")
}

/// Opens the socket of an agent's MCP bridge below the directory of the main
/// socket and accepts its clients on a background thread. Returns its path.
pub fn bridge_listener(main: &Path, agent: &str, tx: mpsc::Sender<Msg>) -> anyhow::Result<PathBuf> {
    let path = main.parent().unwrap_or(Path::new(".")).join("bridges").join(format!("{agent}.sock"));
    let listener = bind(&path)?;
    let scope = agent.to_string();
    thread::Builder::new()
        .name(format!("bridge-{agent}"))
        .spawn(move || accept_loop(listener, tx, Some(scope)))
        .context("cannot start the accept thread of the bridge socket")?;
    Ok(path)
}

/// Client ids are unique across all sockets.
static NEXT_CLIENT: AtomicU64 = AtomicU64::new(1);

/// `scope` is the agent of a bridge socket, None for the main socket.
fn accept_loop(listener: UnixListener, tx: mpsc::Sender<Msg>, scope: Option<String>) {
    for stream in listener.incoming() {
        let stream = match stream {
            Ok(s) => s,
            Err(e) => {
                warn!("accept failed ({e})");
                thread::sleep(Duration::from_millis(100));
                continue;
            }
        };
        let id = NEXT_CLIENT.fetch_add(1, Ordering::SeqCst);
        let (tx, scope) = (tx.clone(), scope.clone());
        if let Err(e) = thread::Builder::new().name(format!("client-{id}")).spawn(move || client_loop(id, stream, tx, scope)) {
            warn!("cannot start a thread for client {id} ({e})");
        }
    }
}

fn client_loop(id: u64, stream: UnixStream, tx: mpsc::Sender<Msg>, scope: Option<String>) {
    let writer = match stream.try_clone() {
        Ok(w) => w,
        Err(e) => {
            warn!("cannot clone the stream of client {id} ({e})");
            return;
        }
    };
    if tx.send(Msg::Connected { id, stream: writer, scoped: scope.is_some() }).is_err() {
        return;
    }
    for line in BufReader::new(stream).lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        match decode_command(&line) {
            Ok(command) => {
                let Some(command) = scoped(scope.as_deref(), command) else {
                    warn!(
                        "client {id} on the bridge socket of {} sent a command that bridges may not send",
                        scope.as_deref().unwrap_or("?")
                    );
                    continue;
                };
                if tx.send(Msg::Command { id, command }).is_err() {
                    return;
                }
            }
            Err(e) => warn!("client {id} sent {line:?}, which is not a command ({e})"),
        }
    }
    let _ = tx.send(Msg::Gone { id });
}

/// A client of a bridge socket may only send hello and tool calls, which
/// always count as calls of the socket's agent.
fn scoped(scope: Option<&str>, command: Command) -> Option<Command> {
    match (scope, command) {
        (None, command) => Some(command),
        (Some(_), hello @ Command::Hello { .. }) => Some(hello),
        (Some(agent), Command::Tool { id, call, .. }) => Some(Command::Tool { id, agent: agent.to_string(), call }),
        (Some(_), _) => None,
    }
}

fn daemon_loop(mut daemon: Daemon, rx: mpsc::Receiver<Msg>) {
    let mut hub = Hub::default();
    loop {
        daemon.tick();
        hub.send(daemon.take_out());
        let msg = match rx.recv_timeout(daemon.next_wait()) {
            Ok(msg) => msg,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => return,
        };
        match msg {
            Msg::Connected { id, stream, scoped } => hub.connect(id, stream, scoped),
            Msg::Gone { id } => {
                hub.disconnect(id);
                // The app client may have left (app_tools.rs).
                daemon.client_gone(id);
            }
            Msg::Command { id, command: Command::Hello { version, client } } => {
                info!("client {id} says hello as {client}");
                if version != PROTOCOL_VERSION {
                    warn!("client {id} speaks protocol version {version}, the daemon speaks {PROTOCOL_VERSION}");
                }
                daemon.tick();
                hub.send(daemon.take_out());
                if hub.is_scoped(id) {
                    hub.greet(id, &[Event::Hello { version: PROTOCOL_VERSION, daemon: DAEMON_NAME.into() }]);
                } else {
                    hub.greet(id, &daemon.greeting());
                }
            }
            msg => daemon.handle_msg(msg),
        }
        hub.send(daemon.take_out());
    }
}

struct Client {
    stream: UnixStream,
    /// Only clients that said hello receive broadcasts, so the snapshot always
    /// comes before the first change.
    greeted: bool,
    /// A client of a bridge socket, which gets no broadcasts at all.
    scoped: bool,
}

#[derive(Default)]
struct Hub {
    clients: HashMap<u64, Client>,
}

impl Hub {
    fn connect(&mut self, id: u64, stream: UnixStream, scoped: bool) {
        if let Err(e) = stream.set_write_timeout(Some(WRITE_TIMEOUT)) {
            warn!("cannot set a write timeout for client {id} ({e})");
        }
        debug!("client {id} connected");
        self.clients.insert(id, Client { stream, greeted: false, scoped });
    }

    fn is_scoped(&self, id: u64) -> bool {
        self.clients.get(&id).is_some_and(|c| c.scoped)
    }

    fn disconnect(&mut self, id: u64) {
        if self.clients.remove(&id).is_some() {
            info!("client {id} left");
        }
    }

    fn greet(&mut self, id: u64, events: &[Event]) {
        if let Some(client) = self.clients.get_mut(&id) {
            client.greeted = true;
        }
        self.send(events.iter().cloned().map(|e| Outgoing::To(id, e)).collect());
    }

    /// Writes the events, batched into one write per client.
    fn send(&mut self, outgoing: Vec<Outgoing>) {
        if outgoing.is_empty() {
            return;
        }
        let mut payloads: HashMap<u64, String> = HashMap::new();
        for item in &outgoing {
            match item {
                Outgoing::All(event) => {
                    let line = encode(event);
                    debug!("broadcast {}", line.trim_end());
                    for (id, client) in &self.clients {
                        if client.greeted && !client.scoped {
                            payloads.entry(*id).or_default().push_str(&line);
                        }
                    }
                }
                Outgoing::To(id, event) => {
                    if self.clients.contains_key(id) {
                        payloads.entry(*id).or_default().push_str(&encode(event));
                    }
                }
            }
        }
        let mut failed = Vec::new();
        for (id, payload) in payloads {
            if let Some(client) = self.clients.get_mut(&id)
                && let Err(e) = client.stream.write_all(payload.as_bytes())
            {
                failed.push((id, e));
            }
        }
        for (id, e) in failed {
            self.drop_client(id, &e);
        }
    }

    fn drop_client(&mut self, id: u64, error: &io::Error) {
        if let Some(client) = self.clients.remove(&id) {
            match error.kind() {
                // The command line clients close right after their command.
                io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset => info!("client {id} left"),
                _ => warn!("dropped client {id} ({error})"),
            }
            let _ = client.stream.shutdown(Shutdown::Both);
        }
    }
}

/// A connection of a command line client that said hello.
struct Connection {
    stream: UnixStream,
    reader: BufReader<UnixStream>,
}

impl Connection {
    fn open(socket: &Path, name: &str) -> anyhow::Result<Self> {
        let mut stream =
            UnixStream::connect(socket).with_context(|| format!("cannot connect to {}. Is meerora-agentd running?", socket.display()))?;
        stream.set_read_timeout(Some(HELLO_TIMEOUT))?;
        let hello = Command::Hello { version: PROTOCOL_VERSION, client: format!("{DAEMON_NAME} ({name})") };
        stream.write_all(encode(&hello).as_bytes())?;
        let mut reader = BufReader::new(stream.try_clone()?);
        let mut line = String::new();
        reader.read_line(&mut line).context("no answer from the daemon")?;
        match decode_event(&line) {
            Ok(Event::Hello { version, daemon }) => {
                if version != PROTOCOL_VERSION {
                    warn!("{daemon} speaks protocol version {version}, this tool speaks {PROTOCOL_VERSION}");
                }
            }
            _ => bail!("the daemon did not answer with hello but with {:?}", line.trim_end()),
        }
        Ok(Self { stream, reader })
    }

    fn send(&mut self, command: &Command) -> anyhow::Result<()> {
        self.stream.write_all(encode(command).as_bytes())?;
        Ok(())
    }

    /// Reads and drops what arrives within `quiet`, which is the snapshot after hello.
    fn skip_snapshot(&mut self, quiet: Duration) -> anyhow::Result<()> {
        self.stream.set_read_timeout(Some(quiet))?;
        let mut line = String::new();
        loop {
            line.clear();
            match self.reader.read_line(&mut line) {
                Ok(0) => bail!("the daemon closed the connection"),
                Ok(_) => {}
                Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => return Ok(()),
                Err(e) => return Err(e.into()),
            }
        }
    }
}

/// Connects to the daemon, says hello, waits for its hello and sends `command`.
pub fn send_command(socket: &Path, command: &Command) -> anyhow::Result<()> {
    let mut connection = Connection::open(socket, "command line")?;
    connection.send(command)?;
    connection.stream.shutdown(Shutdown::Write)?;
    Ok(())
}

/// Sends a message to an agent and prints the agent's events as JSON lines
/// until its run finishes. A mock agent answers with a system message.
pub fn say(socket: &Path, agent: &str, text: &str, out: &mut dyn Write) -> anyhow::Result<()> {
    let mut connection = Connection::open(socket, "say")?;
    connection.skip_snapshot(Duration::from_millis(300))?;
    connection.send(&Command::Say { agent: agent.into(), text: text.into() })?;
    connection.stream.set_read_timeout(Some(SAY_TIMEOUT))?;
    let mut started = false;
    let mut line = String::new();
    loop {
        line.clear();
        if connection.reader.read_line(&mut line).context("no event from the daemon")? == 0 {
            bail!("the daemon closed the connection");
        }
        let Ok(event) = decode_event(&line) else { continue };
        if event.agent() != Some(agent) {
            continue;
        }
        out.write_all(line.as_bytes())?;
        out.flush()?;
        match event {
            Event::RunStarted { .. } => started = true,
            // A run that was going before the message finishes first and is not ours.
            Event::RunFinished { .. } if started => return Ok(()),
            Event::Message { role: Role::System, .. } if !started => return Ok(()),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bridge_sockets_only_take_tool_calls_of_their_agent() {
        use meerora_protocol::ToolCall;
        let note = |agent: &str| Command::Tool { id: "t".into(), agent: agent.into(), call: ToolCall::Note { text: "hi".into() } };
        assert_eq!(scoped(Some("wren"), note("claude")), Some(note("wren")), "a bridge speaks only for its agent");
        let hello = Command::Hello { version: PROTOCOL_VERSION, client: "meerora-mcp".into() };
        assert_eq!(scoped(Some("wren"), hello.clone()), Some(hello));
        for refused in [
            Command::DecideCapability { id: "cap-1".into(), approve: true, remember: true },
            Command::DecideConfig { id: "config-1".into(), apply: true },
            Command::RevokeCapability { agent: "wren".into(), capability: "web".into() },
            Command::Say { agent: "claude".into(), text: "hi".into() },
            Command::Inject { event: Event::GoTo { agent: "wren".into(), station: "door".into() } },
            Command::ReloadAgents,
            Command::ServeApps,
            Command::AppReply { id: "app-1".into(), result: meerora_protocol::AppResult::text("ok") },
        ] {
            assert_eq!(scoped(Some("wren"), refused.clone()), None, "{refused:?}");
            assert_eq!(scoped(None, refused.clone()), Some(refused), "the main socket takes everything");
        }
    }

    #[test]
    fn clock_scales_time() {
        let fast = Clock { start: Instant::now() - Duration::from_secs(10), speed: 2.0 };
        assert!((fast.now() - 20.0).abs() < 0.5);
        assert!(fast.until(10.0).is_zero());
        let wait = fast.until(30.0).as_secs_f64();
        assert!((4.5..=5.0).contains(&wait), "{wait}");
        assert_eq!(fast.until(1e12), Duration::from_secs_f64(MAX_SLEEP));
    }
}
