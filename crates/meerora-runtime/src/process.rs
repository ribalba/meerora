//! Child processes of harness runtimes: finding the binary, starting it in
//! its own process group, reading its JSON lines and cancelling it.

use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::{RunEvent, RunHandle};

/// How long a cancelled child gets between SIGTERM and SIGKILL.
const KILL_GRACE: Duration = Duration::from_secs(2);
/// How much of a child's stderr is kept for error messages.
const STDERR_TAIL: usize = 8 * 1024;

const SIGKILL: i32 = 9;
const SIGTERM: i32 = 15;

unsafe extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}

/// Sends `signal` to every process in the group `pgid`.
pub(crate) fn signal_group(pgid: u32, signal: i32) {
    let Ok(pid) = i32::try_from(pgid) else { return };
    if pid > 1 {
        // SAFETY: kill(2) only takes integers and has no memory effects.
        unsafe {
            kill(-pid, signal);
        }
    }
}

pub(crate) fn kill_group(pgid: u32) {
    signal_group(pgid, SIGKILL);
}

/// Sends events of one run and guarantees that exactly one
/// [`RunEvent::Finished`] goes out, after which nothing else does.
pub(crate) struct Emitter {
    tx: Mutex<Option<Sender<RunEvent>>>,
}

impl Emitter {
    pub fn new(tx: Sender<RunEvent>) -> Arc<Self> {
        Arc::new(Self { tx: Mutex::new(Some(tx)) })
    }

    /// Sends `event` unless the run has finished. A dropped receiver is fine.
    pub fn emit(&self, event: RunEvent) {
        if let Some(tx) = self.tx.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            let _ = tx.send(event);
        }
    }

    /// Sends the `Finished` event. Later calls do nothing.
    pub fn finish(&self, result: Result<String, String>) {
        if let Some(tx) = self.tx.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = tx.send(RunEvent::Finished { result });
        }
    }
}

/// A handle whose run has already failed, for errors found before anything started.
pub(crate) fn failed_handle(message: String) -> RunHandle {
    let (tx, rx) = mpsc::channel();
    let _ = tx.send(RunEvent::Finished { result: Err(message) });
    RunHandle::new(rx, Box::new(|| {}))
}

/// Finds a runtime binary: the configured path, or `name` on PATH.
pub(crate) fn find_binary(name: &str, configured: Option<&Path>) -> Result<PathBuf, String> {
    match configured {
        Some(path) if path.components().count() > 1 => {
            if is_executable(path) {
                Ok(path.to_path_buf())
            } else {
                Err(format!("{name} is not installed (looked for {})", path.display()))
            }
        }
        Some(bare) => {
            let bare = bare.to_string_lossy();
            search_path(&bare).ok_or_else(|| format!("{name} is not installed (looked for {bare} on PATH)"))
        }
        None => search_path(name).ok_or_else(|| format!("{name} is not installed (looked for {name} on PATH)")),
    }
}

fn search_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|dir| dir.join(name)).find(|candidate| is_executable(candidate))
}

fn is_executable(path: &Path) -> bool {
    path.metadata().map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0).unwrap_or(false)
}

/// Checks the agent directory before a child is started in it.
pub(crate) fn check_dir(dir: &Path) -> Result<(), String> {
    if dir.is_dir() { Ok(()) } else { Err(format!("agent directory {} does not exist", dir.display())) }
}

/// What to start.
pub(crate) struct ChildSpec {
    /// Short name for messages, for example "claude".
    pub label: &'static str,
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub dir: PathBuf,
    /// Written to stdin, which is closed afterwards.
    pub stdin: String,
    pub env_remove: Vec<OsString>,
    pub env_set: Vec<(String, String)>,
    /// The program is bwrap, which runs the real one in the sandbox
    /// ([`crate::sandbox::wrap`]). Errors of bwrap itself are explained.
    pub sandboxed: bool,
}

/// Turns the stdout lines of one runtime into [`RunEvent`]s.
pub(crate) trait StreamParser: Send + 'static {
    fn line(&mut self, line: &str, emit: &mut dyn FnMut(RunEvent));
    /// Called once after the child exited. Returns the result of the run.
    fn finish(&mut self, status: ExitStatus, stderr: &str, emit: &mut dyn FnMut(RunEvent)) -> Result<String, String>;
}

/// Starts the child and a thread that parses its output. Errors are
/// reported as `Finished`, so this never fails.
pub(crate) fn run_child<P: StreamParser>(spec: ChildSpec, parser: P) -> RunHandle {
    if let Err(message) = check_dir(&spec.dir) {
        return failed_handle(message);
    }
    let mut command = Command::new(&spec.program);
    command.args(&spec.args).current_dir(&spec.dir).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).process_group(0);
    if spec.sandboxed {
        // Only an allowlist of the daemon's variables enters the sandbox (D16).
        command.env_clear().envs(crate::sandbox::kept_env(true));
    }
    for key in &spec.env_remove {
        command.env_remove(key);
    }
    for (key, value) in &spec.env_set {
        command.env(key, value);
    }
    tracing::debug!(program = %spec.program.display(), args = ?spec.args, "starting runtime child");
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && spec.sandboxed => {
            return failed_handle(format!(
                "bubblewrap is not installed (looked for {}), so {} cannot start in the sandbox",
                spec.program.display(),
                spec.label
            ));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return failed_handle(format!("{} is not installed (looked for {})", spec.label, spec.program.display()));
        }
        Err(e) => return failed_handle(format!("could not start {} ({}): {e}", spec.label, spec.program.display())),
    };

    let (tx, rx) = mpsc::channel();
    let emitter = Emitter::new(tx);
    let pgid = child.id();
    let alive = Arc::new(Mutex::new(true));
    let cancelled = Arc::new(AtomicBool::new(false));

    if let Some(mut stdin) = child.stdin.take() {
        let text = spec.stdin;
        std::thread::spawn(move || {
            let _ = stdin.write_all(text.as_bytes());
        });
    }
    let stderr_thread = child.stderr.take().map(|stderr| std::thread::spawn(move || read_tail(stderr, STDERR_TAIL)));
    let stdout = child.stdout.take();

    // Reaps the child, then kills what is left of its group. Leftovers would
    // keep the pipes open, and the run would never see the end of its output.
    let (status_tx, status_rx) = mpsc::channel();
    {
        let alive = alive.clone();
        std::thread::spawn(move || {
            let status = child.wait();
            *alive.lock().unwrap_or_else(|e| e.into_inner()) = false;
            kill_group(pgid);
            let _ = status_tx.send(status);
        });
    }

    {
        let emitter = emitter.clone();
        let cancelled = cancelled.clone();
        let label = spec.label;
        let sandboxed = spec.sandboxed;
        let mut parser = parser;
        std::thread::spawn(move || {
            let mut emit = |event: RunEvent| emitter.emit(event);
            if let Some(stdout) = stdout {
                let mut reader = BufReader::new(stdout);
                let mut buf = Vec::new();
                loop {
                    buf.clear();
                    match reader.read_until(b'\n', &mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {
                            let line = String::from_utf8_lossy(&buf);
                            let line = line.trim();
                            if !line.is_empty() {
                                parser.line(line, &mut emit);
                            }
                        }
                    }
                }
            }
            let status = status_rx.recv().unwrap_or_else(|_| Err(std::io::Error::other("the waiter thread stopped")));
            let stderr = stderr_thread.and_then(|t| t.join().ok()).unwrap_or_default();
            let result = match status {
                Ok(status) if cancelled.load(Ordering::SeqCst) => {
                    let _ = parser.finish(status, &stderr, &mut |_| {});
                    Err("cancelled".to_string())
                }
                Ok(status) => parser.finish(status, &stderr, &mut emit),
                Err(e) => Err(format!("could not wait for {label}: {e}")),
            };
            let result = match result {
                Err(e) if sandboxed && !cancelled.load(Ordering::SeqCst) => Err(crate::sandbox::explain(&stderr).unwrap_or(e)),
                other => other,
            };
            emitter.finish(result);
        });
    }

    let cancel = move || {
        cancelled.store(true, Ordering::SeqCst);
        if *alive.lock().unwrap_or_else(|e| e.into_inner()) {
            signal_group(pgid, SIGTERM);
            let alive = alive.clone();
            std::thread::spawn(move || {
                std::thread::sleep(KILL_GRACE);
                if *alive.lock().unwrap_or_else(|e| e.into_inner()) {
                    kill_group(pgid);
                }
            });
        }
        emitter.finish(Err("cancelled".to_string()));
    };
    RunHandle::new(rx, Box::new(cancel))
}

/// Reads everything and keeps the last `limit` bytes.
fn read_tail(mut source: impl Read, limit: usize) -> String {
    let mut tail: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match source.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                tail.extend_from_slice(&chunk[..n]);
                if tail.len() > 2 * limit {
                    tail.drain(..tail.len() - limit);
                }
            }
        }
    }
    if tail.len() > limit {
        tail.drain(..tail.len() - limit);
    }
    String::from_utf8_lossy(&tail).into_owned()
}

/// The last meaningful lines of a child's stderr, for an error message.
pub(crate) fn stderr_summary(stderr: &str, skip: impl Fn(&str) -> bool) -> Option<String> {
    let lines: Vec<&str> = stderr.lines().map(str::trim).filter(|l| !l.is_empty() && !skip(l)).collect();
    let start = lines.len().saturating_sub(3);
    let text = lines[start..].join(" ");
    if text.is_empty() {
        return None;
    }
    let mut short: String = text.chars().take(500).collect();
    if short.len() < text.len() {
        short.push('…');
    }
    Some(short)
}

/// "exited with status 1", "was killed by signal 9".
pub(crate) fn describe_exit(status: ExitStatus) -> String {
    use std::os::unix::process::ExitStatusExt;
    match (status.code(), status.signal()) {
        (Some(code), _) => format!("exited with status {code}"),
        (None, Some(signal)) => format!("was killed by signal {signal}"),
        _ => "exited".to_string(),
    }
}

/// Cancel state of a run that this crate drives itself: a flag plus the
/// process groups to kill (the MCP bridge, a running command).
#[derive(Default)]
pub(crate) struct CancelState {
    cancelled: AtomicBool,
    groups: Mutex<Vec<u32>>,
}

impl CancelState {
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        for pgid in self.groups.lock().unwrap_or_else(|e| e.into_inner()).iter() {
            kill_group(*pgid);
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    /// Kills `pgid` on cancel, or right away when the run is already cancelled.
    pub fn register(&self, pgid: u32) {
        let mut groups = self.groups.lock().unwrap_or_else(|e| e.into_inner());
        groups.push(pgid);
        if self.is_cancelled() {
            kill_group(pgid);
        }
    }

    pub fn unregister(&self, pgid: u32) {
        self.groups.lock().unwrap_or_else(|e| e.into_inner()).retain(|g| *g != pgid);
    }
}
