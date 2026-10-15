//! Connection to the agent daemon (D16) over its Unix socket. A background
//! thread connects, reconnects every second when the daemon is gone, and
//! forwards events to the UI thread.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command as Process, Stdio};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use meerora_protocol::{Command, Event, PROTOCOL_VERSION, decode_event, encode};

pub enum LinkMsg {
    Connected,
    Event(Event),
    Disconnected,
}

pub struct DaemonLink {
    pub rx: Receiver<LinkMsg>,
    writer: Arc<Mutex<Option<UnixStream>>>,
    spawned: Option<Child>,
}

impl DaemonLink {
    /// Starts the connection thread. `wake` runs after every message so the
    /// event loop can redraw. With `spawn` set, a `meerora-agentd` binary next
    /// to this executable is started when no daemon answers.
    pub fn start(socket: PathBuf, data_dir: &Path, spawn: bool, wake: impl Fn() + Send + 'static) -> Self {
        let spawned = if spawn && UnixStream::connect(&socket).is_err() { spawn_daemon(&socket, data_dir) } else { None };
        let (tx, rx) = channel();
        let writer = Arc::new(Mutex::new(None));
        let thread_writer = writer.clone();
        std::thread::Builder::new()
            .name("agentd-link".into())
            .spawn(move || connection_loop(&socket, &tx, &thread_writer, &wake))
            .expect("spawning the daemon link thread");
        Self { rx, writer, spawned }
    }

    pub fn send(&self, command: &Command) {
        if let Some(stream) = self.writer.lock().expect("daemon writer lock").as_mut()
            && let Err(e) = stream.write_all(encode(command).as_bytes())
        {
            tracing::warn!("sending to the agent daemon failed: {e}");
        }
    }
}

impl Drop for DaemonLink {
    fn drop(&mut self) {
        if let Some(child) = &mut self.spawned {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn connection_loop(socket: &Path, tx: &Sender<LinkMsg>, writer: &Mutex<Option<UnixStream>>, wake: &dyn Fn()) {
    let mut announced_missing = false;
    loop {
        let stream = match UnixStream::connect(socket) {
            Ok(s) => s,
            Err(e) => {
                if !announced_missing {
                    tracing::info!("agent daemon not reachable at {}: {e}", socket.display());
                    announced_missing = true;
                }
                std::thread::sleep(Duration::from_secs(1));
                continue;
            }
        };
        announced_missing = false;
        let Ok(mut write_half) = stream.try_clone() else { continue };
        let hello = Command::Hello { version: PROTOCOL_VERSION, client: format!("meerora {}", env!("CARGO_PKG_VERSION")) };
        if write_half.write_all(encode(&hello).as_bytes()).is_err() {
            continue;
        }
        // This client holds the agent views, so app requests of agents come here (D30).
        if write_half.write_all(encode(&Command::ServeApps).as_bytes()).is_err() {
            continue;
        }
        *writer.lock().expect("daemon writer lock") = Some(write_half);
        if tx.send(LinkMsg::Connected).is_err() {
            return;
        }
        wake();
        for line in BufReader::new(stream).lines() {
            let Ok(line) = line else { break };
            if line.trim().is_empty() {
                continue;
            }
            match decode_event(&line) {
                Ok(event) => {
                    if tx.send(LinkMsg::Event(event)).is_err() {
                        return;
                    }
                    wake();
                }
                Err(e) => tracing::warn!("ignoring malformed daemon line: {e}: {line}"),
            }
        }
        *writer.lock().expect("daemon writer lock") = None;
        if tx.send(LinkMsg::Disconnected).is_err() {
            return;
        }
        wake();
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Starts `meerora-agentd` from the same directory as this executable,
/// logging to a file next to the socket.
fn spawn_daemon(socket: &Path, data_dir: &Path) -> Option<Child> {
    let exe = std::env::current_exe().ok()?.with_file_name("meerora-agentd");
    if !exe.is_file() {
        tracing::info!("no agent daemon running and {} not found. Build it with `cargo build`.", exe.display());
        return None;
    }
    let log_path = socket.with_file_name("agentd.log");
    if let Some(dir) = log_path.parent() {
        use std::os::unix::fs::DirBuilderExt;
        let _ = std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir);
    }
    let log = std::fs::File::create(&log_path).ok()?;
    let child = Process::new(&exe)
        .arg("--data-dir")
        .arg(data_dir)
        .arg("--socket")
        .arg(socket)
        .stdin(Stdio::null())
        .stdout(log.try_clone().ok()?)
        .stderr(log)
        .spawn();
    match child {
        Ok(child) => {
            tracing::info!("started {} (log: {})", exe.display(), log_path.display());
            Some(child)
        }
        Err(e) => {
            tracing::warn!("could not start {}: {e}", exe.display());
            None
        }
    }
}
