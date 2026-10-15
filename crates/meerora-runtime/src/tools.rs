//! Tools of model runtimes, which this crate runs itself (D29): files of the
//! agent directory, and with `shell` commands inside it.
//!
//! Every path is resolved against the canonical agent directory, and absolute
//! paths may also point into the agent's workspace folders. Symlinks
//! are followed before the check, so a link that points outside is refused
//! like `../` or an absolute path. Writes into `.git` are refused too, since
//! the daemon commits the directory (D21) and git runs its hooks.
//!
//! In agent runs every command runs inside the sandbox (sandbox.rs, D16), so
//! a command sees the agent directory and the system, not the user's home.

use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::Permissions;
use crate::process::{CancelState, kill_group};
use crate::sandbox::{self, Mounts};

pub(crate) const READ_LIMIT: usize = 100 * 1024;
pub(crate) const OUTPUT_LIMIT: usize = 16 * 1024;
pub(crate) const COMMAND_TIMEOUT: Duration = Duration::from_secs(60);
const LIST_LIMIT: usize = 500;

/// The agent directory and its workspace folders as the only places the
/// file tools reach.
pub(crate) struct AgentFiles {
    root: PathBuf,
    /// Workspace folders, canonical. Relative paths still mean the agent directory.
    workspace: Vec<PathBuf>,
    /// bwrap and the mounts that commands run with, in agent runs.
    sandbox: Option<(PathBuf, Mounts)>,
}

impl AgentFiles {
    pub fn new(dir: &Path) -> Result<Self, String> {
        let root = dir.canonicalize().map_err(|e| format!("agent directory {}: {e}", dir.display()))?;
        Ok(Self { root, workspace: Vec::new(), sandbox: None })
    }

    /// Lets the file tools reach the workspace folders too. Missing ones are skipped.
    pub fn with_workspace(mut self, folders: &[PathBuf]) -> Self {
        self.workspace = folders.iter().filter_map(|f| f.canonicalize().ok()).collect();
        self
    }

    /// Runs every command inside the sandbox from now on.
    pub fn sandboxed(mut self, bwrap: PathBuf, mounts: Mounts) -> Self {
        self.sandbox = Some((bwrap, mounts));
        self
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolves `path` (relative to the agent directory, or absolute) to a
    /// canonical path inside it. The path may not exist yet, but its existing
    /// part decides, with symlinks resolved.
    pub fn resolve(&self, path: &str) -> Result<PathBuf, String> {
        let trimmed = path.trim();
        let also = if self.workspace.is_empty() { "" } else { " and its workspace folders" };
        let outside = || format!("{trimmed} is outside the agent directory{also}");
        let raw = if trimmed.is_empty() || trimmed == "." {
            self.root.clone()
        } else if Path::new(trimmed).is_absolute() {
            PathBuf::from(trimmed)
        } else {
            self.root.join(trimmed)
        };
        let mut existing = raw.as_path();
        let mut missing = Vec::new();
        while existing.symlink_metadata().is_err() {
            match (existing.parent(), existing.file_name()) {
                (Some(parent), Some(name)) => {
                    missing.push(name.to_os_string());
                    existing = parent;
                }
                _ => return Err(outside()),
            }
        }
        // A dangling symlink fails here, so nothing is ever created through one.
        let mut resolved = existing.canonicalize().map_err(|_| outside())?;
        if !resolved.starts_with(&self.root) && !self.workspace.iter().any(|w| resolved.starts_with(w)) {
            return Err(outside());
        }
        for name in missing.iter().rev() {
            resolved.push(name);
        }
        Ok(resolved)
    }

    /// `path` relative to the agent directory or the workspace folder it lies in.
    fn relative<'a>(&self, path: &'a Path) -> &'a Path {
        std::iter::once(&self.root).chain(&self.workspace).find_map(|r| path.strip_prefix(r).ok()).unwrap_or(path)
    }

    pub fn read_file(&self, path: &str) -> Result<String, String> {
        let full = self.resolve(path)?;
        if !full.is_file() {
            return Err(format!("{path} is not a file"));
        }
        let mut bytes = Vec::new();
        std::fs::File::open(&full)
            .and_then(|f| f.take(READ_LIMIT as u64 + 1).read_to_end(&mut bytes))
            .map_err(|e| format!("cannot read {path}: {e}"))?;
        let truncated = bytes.len() > READ_LIMIT;
        bytes.truncate(READ_LIMIT);
        let mut text = String::from_utf8_lossy(&bytes).into_owned();
        if truncated {
            text.push_str(&format!("\n[cut after {} KiB]", READ_LIMIT / 1024));
        }
        Ok(text)
    }

    pub fn write_file(&self, path: &str, content: &str) -> Result<String, String> {
        let full = self.resolve(path)?;
        // The files meerora reads from the agent directory (crate::sandbox::PROTECTED).
        if full.starts_with(&self.root)
            && let Some(Component::Normal(first)) = self.relative(&full).components().next()
            && crate::sandbox::PROTECTED.iter().any(|p| first == std::ffi::OsStr::new(p))
        {
            return Err(format!("{path} belongs to meerora (settings, git data, state and logs), the agent cannot write it"));
        }
        if self.relative(&full).components().any(|c| c == Component::Normal(".git".as_ref())) {
            return Err(format!("{path} is inside .git, which only meerora writes"));
        }
        if full == self.root || full.is_dir() {
            return Err(format!("{path} is a directory"));
        }
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("cannot create the folder for {path}: {e}"))?;
        }
        std::fs::write(&full, content).map_err(|e| format!("cannot write {path}: {e}"))?;
        Ok(format!("wrote {} bytes to {}", content.len(), self.relative(&full).display()))
    }

    pub fn list_files(&self, path: &str) -> Result<String, String> {
        let full = self.resolve(path)?;
        let entries = std::fs::read_dir(&full).map_err(|e| format!("cannot list {}: {e}", if path.is_empty() { "." } else { path }))?;
        let mut names: Vec<String> = entries
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name() != ".git")
            .map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                if e.file_type().map(|t| t.is_dir()).unwrap_or(false) { format!("{name}/") } else { name }
            })
            .collect();
        names.sort();
        let total = names.len();
        names.truncate(LIST_LIMIT);
        let mut text = names.join("\n");
        if total > LIST_LIMIT {
            text.push_str(&format!("\n[{} more]", total - LIST_LIMIT));
        }
        if text.is_empty() {
            text = "(empty)".to_string();
        }
        Ok(text)
    }

    /// Runs `sh -c command` in the agent directory with a timeout and capped output.
    pub fn run_command(&self, command: &str, cancel: &CancelState) -> Result<String, String> {
        let mut process = match &self.sandbox {
            Some((bwrap, mounts)) => {
                let mut p = Command::new(bwrap);
                p.args(sandbox::args(mounts, Path::new("sh"), &["-c".into(), command.into()]));
                p.env_clear().envs(sandbox::kept_env(false));
                p
            }
            None => {
                let mut p = Command::new("sh");
                p.arg("-c").arg(command);
                p
            }
        };
        let mut child = process
            .current_dir(&self.root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .spawn()
            .map_err(|e| format!("cannot start sh: {e}"))?;
        let pgid = child.id();
        cancel.register(pgid);
        let stdout = child.stdout.take().map(|s| std::thread::spawn(move || read_head(s, OUTPUT_LIMIT)));
        let stderr = child.stderr.take().map(|s| std::thread::spawn(move || read_head(s, OUTPUT_LIMIT)));
        let started = Instant::now();
        let mut timed_out = false;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) => {}
                Err(_) => break None,
            }
            if cancel.is_cancelled() || started.elapsed() > COMMAND_TIMEOUT {
                timed_out = !cancel.is_cancelled();
                kill_group(pgid);
                break child.wait().ok();
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        // Children that kept the pipes open must not hold the run.
        kill_group(pgid);
        cancel.unregister(pgid);
        let out = stdout.and_then(|t| t.join().ok()).unwrap_or_default();
        let err = stderr.and_then(|t| t.join().ok()).unwrap_or_default();
        let mut text = match (timed_out, status.and_then(|s| s.code())) {
            (true, _) => format!("timed out after {} s", COMMAND_TIMEOUT.as_secs()),
            (false, Some(code)) => format!("exit code {code}"),
            (false, None) => "killed".to_string(),
        };
        if !out.is_empty() {
            text.push('\n');
            text.push_str(&out);
        }
        if !err.is_empty() {
            text.push_str("\n[stderr]\n");
            text.push_str(&err);
        }
        Ok(text)
    }
}

/// Reads everything and keeps the first `limit` bytes.
fn read_head(mut source: impl Read, limit: usize) -> String {
    let mut head = Vec::new();
    let mut chunk = [0u8; 4096];
    let mut dropped = 0usize;
    loop {
        match source.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let room = limit.saturating_sub(head.len());
                head.extend_from_slice(&chunk[..n.min(room)]);
                dropped += n.saturating_sub(room);
            }
        }
    }
    let mut text = String::from_utf8_lossy(&head).into_owned();
    if dropped > 0 {
        text.push_str(&format!("\n[cut, {dropped} more bytes]"));
    }
    text
}

/// Tool definitions for `/api/chat`, as permitted.
pub(crate) fn definitions(p: Permissions) -> Vec<Value> {
    let path_param = |description: &str| json!({ "type": "string", "description": description });
    let mut tools = vec![
        function(
            "read_file",
            "Read a text file of your agent directory.",
            json!({ "type": "object", "properties": { "path": path_param("Path relative to the agent directory") }, "required": ["path"] }),
        ),
        function(
            "list_files",
            "List the files and folders in a folder of your agent directory. Folders end with a slash.",
            json!({ "type": "object", "properties": { "path": path_param("Folder relative to the agent directory, \".\" for the top") } }),
        ),
    ];
    if p.write {
        tools.push(function(
            "write_file",
            "Create or replace a text file in your agent directory. Missing folders are created.",
            json!({
                "type": "object",
                "properties": {
                    "path": path_param("Path relative to the agent directory"),
                    "content": { "type": "string", "description": "The complete new content of the file" },
                },
                "required": ["path", "content"],
            }),
        ));
    }
    if p.shell {
        tools.push(function(
            "run_command",
            "Run a shell command (sh -c) in your agent directory. It stops after 60 seconds and long output is cut.",
            json!({ "type": "object", "properties": { "command": { "type": "string", "description": "The command line" } }, "required": ["command"] }),
        ));
    }
    tools
}

pub(crate) fn function(name: &str, description: &str, parameters: Value) -> Value {
    json!({ "type": "function", "function": { "name": name, "description": description, "parameters": parameters } })
}

/// True for the names [`definitions`] can offer.
pub(crate) fn is_builtin(name: &str) -> bool {
    matches!(name, "read_file" | "list_files" | "write_file" | "run_command")
}

/// Runs a built-in tool that the permissions allow. The text goes back to the model.
pub(crate) fn call(files: &AgentFiles, p: Permissions, name: &str, args: &Value, cancel: &CancelState) -> Result<String, String> {
    let arg = |key: &str| args.get(key).and_then(Value::as_str);
    match name {
        "read_file" => files.read_file(arg("path").ok_or("read_file needs a path")?),
        "list_files" => files.list_files(arg("path").unwrap_or(".")),
        "write_file" if p.write => files.write_file(arg("path").ok_or("write_file needs a path")?, arg("content").unwrap_or("")),
        "run_command" if p.shell => files.run_command(arg("command").ok_or("run_command needs a command")?, cancel),
        _ => Err(format!("{name} is not available to this agent")),
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("meerora-tools-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn path_guard_keeps_tools_inside() {
        let base = scratch("guard");
        let agent = base.join("agent");
        let secret = base.join("secret");
        std::fs::create_dir_all(agent.join("memory")).unwrap();
        std::fs::create_dir_all(&secret).unwrap();
        std::fs::write(secret.join("key.txt"), "s3cret").unwrap();
        std::fs::write(agent.join("memory/notes.md"), "hello").unwrap();
        symlink(&secret, agent.join("link-out")).unwrap();
        symlink(agent.join("memory"), agent.join("link-in")).unwrap();
        symlink(base.join("missing"), agent.join("dangling")).unwrap();
        let files = AgentFiles::new(&agent).unwrap();

        assert_eq!(files.read_file("memory/notes.md").unwrap(), "hello");
        assert_eq!(files.read_file("link-in/notes.md").unwrap(), "hello");
        assert_eq!(files.read_file(&files.root().join("memory/notes.md").to_string_lossy()).unwrap(), "hello");
        assert!(files.read_file("memory/../memory/notes.md").is_ok());

        for bad in ["../secret/key.txt", "link-out/key.txt", "/etc/passwd", "memory/../../secret/key.txt", "link-out"] {
            let err = files.read_file(bad).unwrap_err();
            assert!(err.contains("outside the agent directory"), "{bad}: {err}");
        }
        for bad in ["link-out/new.txt", "../escape.txt", "dangling", "dangling/x.txt", "nope/../../escape.txt"] {
            assert!(files.write_file(bad, "x").is_err(), "{bad} was writable");
        }
        assert!(!base.join("escape.txt").exists() && !base.join("missing").exists());
        assert!(files.write_file(".git/hooks/post-commit", "x").unwrap_err().contains(".git"));
        for protected in ["agent.toml", "state.json", "logs/messages.jsonl", "sub/.git/config"] {
            assert!(files.write_file(protected, "x").is_err(), "{protected} was writable");
        }
        assert!(files.list_files("link-out").is_err());

        assert!(files.write_file("notes/today/plan.md", "plan").is_ok());
        assert_eq!(std::fs::read_to_string(agent.join("notes/today/plan.md")).unwrap(), "plan");
        assert!(files.write_file("memory", "x").unwrap_err().contains("directory"));
        let listing = files.list_files(".").unwrap();
        assert!(listing.contains("memory/") && listing.contains("notes/"), "{listing}");
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// Workspace folders are reachable by absolute path, their `.git` is not writable.
    #[test]
    fn workspace_folders_are_reachable() {
        let base = scratch("ws");
        let (agent, ws, other) = (base.join("agent"), base.join("project"), base.join("other"));
        for dir in [agent.clone(), ws.join(".git"), ws.join("src"), other.clone()] {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::fs::write(ws.join("src/lib.rs"), "pub fn a() {}\n").unwrap();
        std::fs::write(other.join("secret.txt"), "s").unwrap();
        let files = AgentFiles::new(&agent).unwrap().with_workspace(std::slice::from_ref(&ws));
        let w = |rel: &str| ws.join(rel).display().to_string();
        assert_eq!(files.read_file(&w("src/lib.rs")).unwrap(), "pub fn a() {}\n");
        assert!(files.write_file(&w("src/new.rs"), "x").unwrap().contains("src/new.rs"));
        assert!(files.write_file(&w(".git/config"), "x").unwrap_err().contains(".git"));
        assert!(files.list_files(&w("")).unwrap().contains("src/"));
        let err = files.read_file(&other.join("secret.txt").display().to_string()).unwrap_err();
        assert!(err.contains("outside the agent directory and its workspace folders"), "{err}");
        assert!(files.write_file("notes.md", "n").is_ok(), "relative paths still mean the agent directory");
        assert!(agent.join("notes.md").is_file());
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn commands_run_in_the_directory_with_limits() {
        let dir = scratch("cmd");
        let files = AgentFiles::new(&dir).unwrap();
        let cancel = CancelState::default();
        let out = files.run_command("pwd; echo oops >&2; exit 3", &cancel).unwrap();
        assert!(out.starts_with("exit code 3"), "{out}");
        assert!(out.contains(&files.root().to_string_lossy().into_owned()), "{out}");
        assert!(out.contains("[stderr]\noops"), "{out}");
        let out = files.run_command("head -c 100000 /dev/zero | tr '\\0' a", &cancel).unwrap();
        assert!(out.len() < OUTPUT_LIMIT + 200 && out.contains("[cut,"), "{}", out.len());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn permissions_decide_the_tools() {
        let names = |p| definitions(p).iter().map(|t| t["function"]["name"].as_str().unwrap().to_string()).collect::<Vec<_>>();
        assert_eq!(names(Permissions::default()), vec!["read_file", "list_files"]);
        assert_eq!(
            names(Permissions { write: true, shell: true, web: true }),
            vec!["read_file", "list_files", "write_file", "run_command"]
        );
        let dir = scratch("perm");
        let files = AgentFiles::new(&dir).unwrap();
        let err = call(&files, Permissions::default(), "write_file", &json!({"path": "a", "content": "b"}), &CancelState::default());
        assert!(err.unwrap_err().contains("not available"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
