//! git for the Git app (ui/apps/git.rs): which git runs where, the status,
//! the texts of a file, hunks applied as patches, commit, push and fetch.
//!
//! Which git runs (see the README, "The Git app"):
//! - The agent's own directory gets the hardened git of the editor
//!   (`review::git`): no global or system configuration, no hooks, no
//!   fsmonitor, attributes only from the empty tree. The agent writes that
//!   work tree, and meerora must never run a command it planted. Commits there
//!   carry your name and email from your global git configuration, read once.
//! - Every other repository is yours, so it gets your normal git with your
//!   configuration: your name, email, signing, credential helpers, SSH agent
//!   and hooks work as in a terminal. Two things differ. `core.fsmonitor` is
//!   off, because the app polls every two seconds and a monitor daemon or a
//!   monitor script in the work tree is not worth that. `GIT_ATTR_SOURCE` is
//!   not set, because ignoring `.gitattributes` would break Git LFS (large
//!   files would be committed whole), line ending rules and binary markers.
//!   An attribute can only name a filter or diff driver that a configuration
//!   defines, and agents cannot write the configuration: the `.git` of a
//!   workspace folder is read-only in their sandbox and your home is hidden.
//! - A repository whose git directory lies in an agent's directory or a
//!   workspace folder without the sandbox protecting it (a nested repository
//!   deeper than one level, for example) is refused, because an agent could
//!   have written its configuration. See [`classify`].
//! - A workspace repository whose `.git` appeared while an agent could write
//!   there (`workspace-trust.json`, meerora-protocol workspace.rs) is
//!   untrusted until you trust it. It is read-only: the status runs with the
//!   hardened git, attributes from no file, no submodules and every filter
//!   and diff driver its configuration names switched off, and a
//!   configuration that includes other files gets no git at all.

use std::ffi::OsString;
use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::{Arc, Mutex};

use meerora_protocol::workspace::{Trust, Verdict};

use crate::ui::editor::review;

/// Files larger than this show no diff.
pub const MAX_TEXT: u64 = 2 * 1024 * 1024;
/// How much push and fetch output is kept.
const MAX_OUTPUT: usize = 64 * 1024;

/// Which git runs, see the module documentation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// The agent's own directory: the hardened git.
    Agent,
    /// Your repository: your git with your configuration.
    User,
    /// A workspace repository whose `.git` appeared while an agent could
    /// write there: read-only with the hardened git, see the module documentation.
    Untrusted,
}

/// Why an untrusted repository refuses to change.
pub const READ_ONLY: &str = "This repository is read-only until you trust it.";

/// Where agents can write: their directories and their workspace folders,
/// all canonical. The Git app collects them from every agent.
#[derive(Clone, Debug, Default)]
pub struct Areas {
    pub agents: Vec<PathBuf>,
    pub workspaces: Vec<PathBuf>,
}

/// An open repository.
#[derive(Clone, Debug)]
pub struct Git {
    /// The top of the work tree, canonical.
    pub top: PathBuf,
    pub git_dir: PathBuf,
    pub mode: Mode,
    /// The agent that could write the `.git` of an untrusted repository when it appeared.
    pub made_by: Option<String>,
    /// Your name and email, for commits in an agent's directory.
    identity: Option<(String, String)>,
    /// More environment for every command. Tests give git a home of its own.
    env: Vec<(OsString, OsString)>,
}

fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// Opens the repository that holds `path`, or says why the app does not.
/// `trust` is the record of `workspace-trust.json`.
pub fn open(path: &Path, areas: &Areas, trust: &Trust, env: &[(OsString, OsString)]) -> Result<Git, String> {
    if !path.is_dir() {
        return Err(format!("{} is not a folder.", path.display()));
    }
    // rev-parse with the hardened git: it runs nothing a configuration names.
    let mut cmd = review::git(path);
    cmd.envs(env.iter().filter(|(k, _)| !k.to_string_lossy().starts_with("GIT_CONFIG")).cloned());
    cmd.args(["rev-parse", "--show-toplevel", "--absolute-git-dir"]);
    let out = cmd.stdin(Stdio::null()).output().map_err(|e| format!("cannot run git ({e})"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        if err.contains("must be run in a work tree") {
            return Err(format!("{} is a bare repository without files to review.", path.display()));
        }
        return Err(format!("{} is not a git repository.", path.display()));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut lines = text.lines();
    let (Some(top), Some(git_dir)) = (lines.next(), lines.next()) else {
        return Err(format!("git did not say where the repository of {} is.", path.display()));
    };
    let (top, git_dir) = (canonical(Path::new(top)), canonical(Path::new(git_dir)));
    let (mode, made_by) = classify(&top, &git_dir, areas, trust)?;
    let mut git = Git { top, git_dir, mode, made_by, identity: None, env: env.to_vec() };
    if mode == Mode::Agent {
        git.identity = git.user_identity();
    }
    Ok(git)
}

/// Which git may run in the repository at `top` with its git directory
/// `git_dir`: the hardened one for an agent's own directory, yours where no
/// agent can write the git directory, a read-only one where a `.git`
/// appeared while an agent could write there (with that agent), none where
/// an agent can write it now.
pub fn classify(top: &Path, git_dir: &Path, areas: &Areas, trust: &Trust) -> Result<(Mode, Option<String>), String> {
    for agent in &areas.agents {
        if git_dir.starts_with(agent) || top.starts_with(agent) {
            if top == agent && git_dir == agent.join(".git") {
                return Ok((Mode::Agent, None));
            }
            return Err("This repository lies inside an agent's directory but is not that directory's own history. An agent could \
                        have written its git settings, so meerora does not run git there."
                .into());
        }
    }
    for folder in &areas.workspaces {
        // A submodule keeps its git directory inside the protected .git of its parent.
        if git_dir.starts_with(folder)
            && !meerora_protocol::workspace::protected_git_dirs(folder).iter().any(|g| git_dir.starts_with(canonical(g)))
        {
            return Err(format!(
                "This repository's .git lies in the workspace folder {}, deeper than the sandbox protects it. An agent could have \
                 written its git settings, so meerora does not run git there. Add the repository itself as a workspace folder, or \
                 use a terminal if you trust it.",
                folder.display()
            ));
        }
    }
    match trust.verdict(&top.join(".git")) {
        Verdict::Trusted => Ok((Mode::User, None)),
        Verdict::Untrusted(agent) => Ok((Mode::Untrusted, Some(agent))),
    }
}

/// The first `error:` or `fatal:` line of a failed command, else its last line.
pub fn failure(what: &str, out: &Output) -> String {
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let line = stderr
        .lines()
        .map(str::trim)
        .find(|l| l.starts_with("error:") || l.starts_with("fatal:"))
        .or_else(|| stderr.lines().chain(stdout.lines()).map(str::trim).rfind(|l| !l.is_empty()))
        .unwrap_or("no output");
    let reason = line.trim_start_matches("error:").trim_start_matches("fatal:").trim();
    format!("git {what} failed: {reason}")
}

impl Git {
    /// A git command in this repository, see the module documentation.
    pub fn cmd(&self) -> Command {
        let mut cmd = match self.mode {
            Mode::Agent | Mode::Untrusted => {
                let mut c = review::git(&self.top);
                c.envs(self.env.iter().filter(|(k, _)| !k.to_string_lossy().starts_with("GIT_CONFIG")).cloned());
                if let Some((name, email)) = &self.identity {
                    c.arg("-c").arg(format!("user.name={name}")).arg("-c").arg(format!("user.email={email}"));
                }
                if self.mode == Mode::Untrusted {
                    // GIT_ATTR_SOURCE covers .gitattributes only, not these.
                    c.args(["-c", "core.attributesFile=/dev/null"]);
                }
                c
            }
            Mode::User => {
                let mut c = Command::new("git");
                c.envs(self.env.iter().cloned())
                    .env_remove("GIT_DIR")
                    .env_remove("GIT_WORK_TREE")
                    .env_remove("GIT_INDEX_FILE")
                    .arg("-C")
                    .arg(&self.top)
                    .args(["-c", "core.fsmonitor=false", "-c", "core.quotepath=false"]);
                c
            }
        };
        // Never wait for a person on a terminal, never open an editor or a pager.
        cmd.env("GIT_TERMINAL_PROMPT", "0").env("GIT_EDITOR", ":").env("GIT_PAGER", "cat").env("GIT_LITERAL_PATHSPECS", "1");
        cmd.stdin(Stdio::null());
        cmd
    }

    fn run(&self, args: &[&str], what: &str) -> Result<Output, String> {
        let mut cmd = self.cmd();
        cmd.args(args);
        let out = cmd.output().map_err(|e| format!("cannot run git ({e})"))?;
        if out.status.success() { Ok(out) } else { Err(failure(what, &out)) }
    }

    /// Runs git with `input` on stdin.
    fn run_with(&self, mut cmd: Command, input: &[u8], what: &str) -> Result<Output, String> {
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = cmd.spawn().map_err(|e| format!("cannot run git ({e})"))?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(input).map_err(|e| format!("git {what} failed ({e})"))?;
        }
        let out = child.wait_with_output().map_err(|e| format!("git {what} failed ({e})"))?;
        if out.status.success() { Ok(out) } else { Err(failure(what, &out)) }
    }

    /// Your name and email from your global git configuration.
    fn user_identity(&self) -> Option<(String, String)> {
        let get = |key: &str| {
            let out = Command::new("git").envs(self.env.iter().cloned()).args(["config", "--global", "--get", key]).output().ok()?;
            let value = String::from_utf8_lossy(&out.stdout).trim().to_string();
            (out.status.success() && !value.is_empty()).then_some(value)
        };
        Some((get("user.name")?, get("user.email")?))
    }

    /// An untrusted repository refuses every change.
    fn writable(&self) -> Result<(), String> {
        if self.mode == Mode::Untrusted { Err(READ_ONLY.into()) } else { Ok(()) }
    }

    /// The key of this repository in `workspace-trust.json`.
    pub fn trust_key(&self) -> PathBuf {
        self.top.join(".git")
    }

    /// A command for an untrusted repository with every filter and diff
    /// driver its configuration names switched off, through
    /// `GIT_CONFIG_COUNT`, which takes any name. An empty command makes git
    /// skip a driver. A configuration that includes other files is refused,
    /// because what those files name can change after this look.
    fn neutralized(&self) -> Result<Command, String> {
        // review::git already switches every named driver off through the
        // shared gitsafe module. Here a configuration that includes other
        // files must stop git before it runs, with a message that says why.
        let mut probe = Command::new("git");
        meerora_protocol::gitsafe::neutralize(&mut probe, &self.top).map_err(|_| {
            "This repository's git configuration includes other files, so meerora runs no git here until you trust it.".to_string()
        })?;
        Ok(self.cmd())
    }

    /// The git configuration and attribute files, read without git, for
    /// "Show .git/config". Each is cut at 64 KB.
    pub fn config_text(&self) -> String {
        let mut out = String::new();
        for path in [self.git_dir.join("config"), self.top.join(".gitattributes"), self.git_dir.join("info/attributes")] {
            let Ok(bytes) = std::fs::read(&path) else { continue };
            let cut = bytes.len() > 64 * 1024;
            out.push_str(&format!("# {}\n{}", path.display(), String::from_utf8_lossy(&bytes[..bytes.len().min(64 * 1024)])));
            if cut {
                out.push_str("\n[cut after 64 KB]");
            }
            if !out.ends_with('\n') {
                out.push('\n');
            }
            out.push('\n');
        }
        if out.is_empty() { "No configuration or attribute files.".into() } else { out }
    }

    /// The name shown for the repository: the folder name of its top.
    pub fn name(&self) -> String {
        self.top.file_name().map_or_else(|| self.top.display().to_string(), |n| n.to_string_lossy().into_owned())
    }

    pub fn status(&self) -> Result<Status, String> {
        let mut cmd = if self.mode == Mode::Untrusted { self.neutralized()? } else { self.cmd() };
        // Status refreshes the index by itself. Without optional locks it
        // never holds index.lock while you run git in a terminal.
        cmd.env("GIT_OPTIONAL_LOCKS", "0").args(["status", "--porcelain=v2", "--branch", "-z", "--untracked-files=all", "--no-renames"]);
        if self.mode == Mode::Untrusted {
            cmd.arg("--ignore-submodules=all");
        }
        let out = cmd.output().map_err(|e| format!("cannot run git ({e})"))?;
        if !out.status.success() {
            return Err(failure("status", &out));
        }
        Ok(parse_status(&out.stdout))
    }

    /// The content of a blob.
    pub fn blob(&self, oid: &str) -> Result<Vec<u8>, String> {
        Ok(self.run(&["cat-file", "blob", oid], "cat-file")?.stdout)
    }

    /// A file of the work tree, or why it is not shown.
    pub fn disk(&self, path: &str) -> Result<Option<Vec<u8>>, String> {
        let full = self.top.join(path);
        match std::fs::symlink_metadata(&full) {
            Err(_) => Ok(None),
            Ok(m) if m.file_type().is_symlink() => {
                Ok(Some(std::fs::read_link(&full).map(|t| t.to_string_lossy().into_owned().into_bytes()).unwrap_or_default()))
            }
            Ok(m) if m.len() > MAX_TEXT => {
                Err(format!("{path} is {} large, too large to show.", crate::ui::editor::files::human_size(m.len())))
            }
            Ok(_) => std::fs::read(&full).map(Some).map_err(|e| format!("cannot read {path} ({e})")),
        }
    }

    pub fn stage(&self, path: &str) -> Result<(), String> {
        self.writable()?;
        self.run(&["add", "-A", "--", path], "add").map(drop)
    }

    pub fn unstage(&self, path: &str, has_head: bool) -> Result<(), String> {
        self.writable()?;
        if has_head {
            self.run(&["restore", "--staged", "--", path], "restore")
        } else {
            self.run(&["rm", "--cached", "-q", "--", path], "rm")
        }
        .map(drop)
    }

    pub fn stage_all(&self) -> Result<(), String> {
        self.writable()?;
        self.run(&["add", "-A"], "add").map(drop)
    }

    pub fn unstage_all(&self, has_head: bool) -> Result<(), String> {
        self.writable()?;
        if has_head {
            self.run(&["restore", "--staged", "--", "."], "restore")
        } else {
            self.run(&["rm", "-r", "--cached", "-q", "--", "."], "rm")
        }
        .map(drop)
    }

    /// Restores a file of the work tree from the index. Nothing goes to the trash.
    pub fn discard(&self, path: &str) -> Result<(), String> {
        self.writable()?;
        self.run(&["restore", "--worktree", "--", path], "restore").map(drop)
    }

    /// Applies a patch to the index (`cached`) or to the work tree, or takes
    /// it back with `reverse`.
    pub fn apply(&self, patch: &str, cached: bool, reverse: bool) -> Result<(), String> {
        self.writable()?;
        let mut cmd = self.cmd();
        cmd.args(["apply", "--whitespace=nowarn"]);
        if cached {
            cmd.arg("--cached");
        }
        if reverse {
            cmd.arg("--reverse");
        }
        cmd.arg("-");
        self.run_with(cmd, patch.as_bytes(), "apply").map(drop)
    }

    /// Commits what is staged with `message`, or replaces the last commit
    /// with `amend`. Without `hooks` the commit runs no hook, which is how
    /// commits an agent asks for run. Returns the short id.
    pub fn commit(&self, message: &str, amend: bool, hooks: bool) -> Result<String, String> {
        self.writable()?;
        if message.trim().is_empty() {
            return Err("A commit needs a message.".into());
        }
        let mut cmd = self.cmd();
        if !hooks {
            cmd.args(["-c", "core.hooksPath=/dev/null"]);
        }
        cmd.args(["commit", "-F", "-"]);
        if amend {
            cmd.arg("--amend");
        }
        self.run_with(cmd, message.trim().as_bytes(), "commit")?;
        Ok(String::from_utf8_lossy(&self.run(&["rev-parse", "--short", "HEAD"], "rev-parse")?.stdout).trim().to_string())
    }

    /// The message of the last commit, for amending.
    pub fn last_message(&self) -> Result<String, String> {
        Ok(String::from_utf8_lossy(&self.run(&["log", "-1", "--format=%B"], "log")?.stdout).trim().to_string())
    }

    pub fn remotes(&self) -> Vec<String> {
        if self.mode == Mode::Untrusted {
            return Vec::new();
        }
        self.run(&["remote"], "remote")
            .map(|o| String::from_utf8_lossy(&o.stdout).lines().map(str::to_string).collect())
            .unwrap_or_default()
    }

    /// The hooks folder relative to the top when it lies inside the work
    /// tree (husky, `.githooks`), where agents can edit it. None for hooks
    /// in `.git` and in an agent's directory, which runs no hooks.
    pub fn hooks_in_tree(&self) -> Option<String> {
        if self.mode != Mode::User {
            return None;
        }
        let out = self.run(&["rev-parse", "--git-path", "hooks"], "rev-parse").ok()?;
        let path = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
        let full = canonical(&if path.is_absolute() { path } else { self.top.join(path) });
        if full.starts_with(&self.top) && !full.starts_with(&self.git_dir) {
            full.strip_prefix(&self.top).ok().map(|p| p.display().to_string()).filter(|p| !p.is_empty())
        } else {
            None
        }
    }

    /// Starts `git push`, or `git push -u <remote> <branch>` with `upstream`.
    pub fn push(&self, upstream: Option<(&str, &str)>) -> Result<Remote, String> {
        self.writable()?;
        let mut args = vec!["push".to_string(), "--progress".to_string()];
        if let Some((remote, branch)) = upstream {
            args.extend(["-u".to_string(), remote.to_string(), branch.to_string()]);
        }
        Remote::start(self, args, Work::Push)
    }

    pub fn fetch(&self) -> Result<Remote, String> {
        self.writable()?;
        Remote::start(self, vec!["fetch".into(), "--progress".into()], Work::Fetch)
    }
}

/// The branch and the files of `git status --porcelain=v2 --branch -z`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Status {
    /// None while HEAD is detached.
    pub branch: Option<String>,
    /// None on a branch without commits.
    pub oid: Option<String>,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub files: Vec<FileStatus>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Tracked,
    Untracked,
    /// Unmerged, with conflicts to resolve.
    Conflict,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileStatus {
    pub path: String,
    /// The index status (`X` of `XY`), `.` for unchanged.
    pub x: char,
    /// The work tree status (`Y`), `.` for unchanged.
    pub y: char,
    pub kind: Kind,
    /// The blob in HEAD, for a conflict "ours". None when there is none.
    pub head: Option<String>,
    /// The blob in the index. None when there is none.
    pub index: Option<String>,
}

impl FileStatus {
    pub fn staged(&self) -> bool {
        self.kind == Kind::Tracked && self.x != '.'
    }

    pub fn changed(&self) -> bool {
        self.kind == Kind::Conflict || (self.kind == Kind::Tracked && self.y != '.')
    }

    pub fn untracked(&self) -> bool {
        self.kind == Kind::Untracked
    }
}

impl Status {
    pub fn has_head(&self) -> bool {
        self.oid.is_some()
    }

    pub fn staged(&self) -> impl Iterator<Item = &FileStatus> {
        self.files.iter().filter(|f| f.staged())
    }

    pub fn changed(&self) -> impl Iterator<Item = &FileStatus> {
        self.files.iter().filter(|f| f.changed())
    }

    pub fn untracked(&self) -> impl Iterator<Item = &FileStatus> {
        self.files.iter().filter(|f| f.untracked())
    }

    pub fn file(&self, path: &str) -> Option<&FileStatus> {
        self.files.iter().find(|f| f.path == path)
    }
}

fn oid(text: &str) -> Option<String> {
    (!text.is_empty() && !text.bytes().all(|b| b == b'0')).then(|| text.to_string())
}

/// Parses `git status --porcelain=v2 --branch -z`.
pub fn parse_status(raw: &[u8]) -> Status {
    let text = String::from_utf8_lossy(raw);
    let mut status = Status::default();
    let mut fields = text.split('\0');
    while let Some(record) = fields.next() {
        if let Some(header) = record.strip_prefix("# ") {
            let (key, value) = header.split_once(' ').unwrap_or((header, ""));
            match key {
                "branch.oid" => status.oid = (value != "(initial)").then(|| value.to_string()),
                "branch.head" => status.branch = (value != "(detached)").then(|| value.to_string()),
                "branch.upstream" => status.upstream = Some(value.to_string()),
                "branch.ab" => {
                    for part in value.split_whitespace() {
                        if let Some(n) = part.strip_prefix('+') {
                            status.ahead = n.parse().unwrap_or(0);
                        } else if let Some(n) = part.strip_prefix('-') {
                            status.behind = n.parse().unwrap_or(0);
                        }
                    }
                }
                _ => {}
            }
            continue;
        }
        let xy = |s: &str| {
            let mut c = s.chars();
            (c.next().unwrap_or('.'), c.next().unwrap_or('.'))
        };
        match record.as_bytes().first() {
            Some(b'1') => {
                let p: Vec<&str> = record.splitn(9, ' ').collect();
                if p.len() == 9 {
                    let (x, y) = xy(p[1]);
                    status.files.push(FileStatus { path: p[8].into(), x, y, kind: Kind::Tracked, head: oid(p[6]), index: oid(p[7]) });
                }
            }
            Some(b'2') => {
                let p: Vec<&str> = record.splitn(10, ' ').collect();
                // The original path follows as a field of its own.
                let _original = fields.next();
                if p.len() == 10 {
                    let (x, y) = xy(p[1]);
                    status.files.push(FileStatus { path: p[9].into(), x, y, kind: Kind::Tracked, head: oid(p[6]), index: oid(p[7]) });
                }
            }
            Some(b'u') => {
                let p: Vec<&str> = record.splitn(11, ' ').collect();
                if p.len() == 11 {
                    let (x, y) = xy(p[1]);
                    status.files.push(FileStatus { path: p[10].into(), x, y, kind: Kind::Conflict, head: oid(p[8]), index: None });
                }
            }
            Some(b'?') if record.len() > 2 => {
                status.files.push(FileStatus { path: record[2..].into(), x: '?', y: '?', kind: Kind::Untracked, head: None, index: None });
            }
            _ => {}
        }
    }
    status
}

/// A path an agent names in `app_act`: relative, inside the repository.
pub fn relative_path(path: &str) -> Result<String, String> {
    let p = Path::new(path.trim());
    let bad = p.as_os_str().is_empty() || p.is_absolute() || p.components().any(|c| !matches!(c, Component::Normal(_) | Component::CurDir));
    if bad { Err(format!("{path:?} is not a path inside the repository")) } else { Ok(path.trim().to_string()) }
}

/// What an agent may do in a Git app (D30, docs/design/agent-apps.md):
/// `stage {path}`, `unstage {path}` and `commit {message}`. Pushing stays
/// with you, and an agent's commits run no hooks, because hooks inside the
/// work tree can be the agent's own code. Runs git on the calling thread.
pub fn agent_act(git: &Git, action: &str, args: &serde_json::Value) -> Result<String, String> {
    let arg = |key: &str| args.get(key).and_then(serde_json::Value::as_str).ok_or_else(|| format!("{action} needs {key}"));
    match action {
        "stage" => {
            let path = relative_path(arg("path")?)?;
            git.stage(&path)?;
            Ok(format!("staged {path}"))
        }
        "unstage" => {
            let path = relative_path(arg("path")?)?;
            let has_head = git.status()?.has_head();
            git.unstage(&path, has_head)?;
            Ok(format!("unstaged {path}"))
        }
        "commit" => {
            let message = arg("message")?;
            if git.status()?.staged().next().is_none() {
                return Err("Nothing is staged, so there is nothing to commit.".into());
            }
            let id = git.commit(message, false, false)?;
            Ok(format!("committed {id}"))
        }
        "push" => Err("Pushing stays with the user. Ask them to push.".into()),
        other => Err(format!("The Git app has no action {other:?}. It knows stage, unstage and commit.")),
    }
}

/// Push or fetch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Work {
    Push,
    Fetch,
}

impl Work {
    pub fn label(self) -> &'static str {
        match self {
            Work::Push => "Push",
            Work::Fetch => "Fetch",
        }
    }
}

/// Output as a terminal would show it: a carriage return starts its line again.
#[derive(Debug, Default)]
pub struct Progress {
    pub text: String,
    cr: bool,
}

impl Progress {
    pub fn push(&mut self, chunk: &str) {
        for ch in chunk.chars() {
            match ch {
                '\r' => self.cr = true,
                '\n' => {
                    self.cr = false;
                    self.text.push('\n');
                }
                ch => {
                    if self.cr {
                        let start = self.text.rfind('\n').map_or(0, |i| i + 1);
                        self.text.truncate(start);
                        self.cr = false;
                    }
                    self.text.push(ch);
                }
            }
        }
        if self.text.len() > MAX_OUTPUT {
            let cut = self.text.len() - MAX_OUTPUT;
            let at = self.text[cut..].find('\n').map_or(cut, |i| cut + i + 1);
            self.text.drain(..at);
        }
    }
}

/// A push or fetch running in the background, with its output.
pub struct Remote {
    pub work: Work,
    pub output: Arc<Mutex<Progress>>,
    done: Receiver<Result<(), String>>,
    pid: i32,
    /// The result once it is there.
    pub result: Option<Result<(), String>>,
}

impl Remote {
    fn start(git: &Git, args: Vec<String>, work: Work) -> Result<Remote, String> {
        let mut cmd = git.cmd();
        cmd.args(&args).stdout(Stdio::piped()).stderr(Stdio::piped());
        // A session of its own, so neither git nor ssh can ask on a
        // terminal that meerora was started from. Stop ends the whole group.
        // SAFETY: setsid is async-signal-safe and touches no memory of the parent.
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        let mut child = cmd.spawn().map_err(|e| format!("cannot run git ({e})"))?;
        let output = Arc::new(Mutex::new(Progress::default()));
        let mut readers = Vec::new();
        for pipe in [child.stdout.take().map(|p| Box::new(p) as Box<dyn Read + Send>), child.stderr.take().map(|p| Box::new(p) as _)]
            .into_iter()
            .flatten()
        {
            let output = output.clone();
            readers.push(std::thread::spawn(move || {
                let mut pipe = pipe;
                let mut buf = [0u8; 4096];
                while let Ok(n) = pipe.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    output.lock().unwrap_or_else(|e| e.into_inner()).push(&String::from_utf8_lossy(&buf[..n]));
                }
            }));
        }
        let pid = child.id() as i32;
        let (tx, done) = mpsc::channel();
        let out = output.clone();
        std::thread::Builder::new()
            .name(format!("git-{}", work.label().to_lowercase()))
            .spawn(move || {
                let status = child.wait();
                for r in readers {
                    let _ = r.join();
                }
                let text = out.lock().unwrap_or_else(|e| e.into_inner()).text.clone();
                let result = match status {
                    Ok(s) if s.success() => Ok(()),
                    Ok(s) => Err(remote_error(work, &text, s.code())),
                    Err(e) => Err(format!("git {} failed ({e})", work.label().to_lowercase())),
                };
                let _ = tx.send(result);
            })
            .map_err(|e| format!("cannot start a thread ({e})"))?;
        Ok(Remote { work, output, done, pid, result: None })
    }

    /// Takes the result once git ended. True while it runs.
    pub fn running(&mut self) -> bool {
        if self.result.is_none() {
            match self.done.try_recv() {
                Ok(r) => self.result = Some(r),
                Err(TryRecvError::Disconnected) => self.result = Some(Err("git stopped without a result".into())),
                Err(TryRecvError::Empty) => {}
            }
        }
        self.result.is_none()
    }

    /// Stops git and whatever it started.
    pub fn stop(&self) {
        if self.result.is_none() && self.pid > 0 {
            // SAFETY: a plain kill(2) on the process group of our own child.
            unsafe {
                libc::kill(-self.pid, libc::SIGTERM);
            }
        }
    }

    pub fn text(&self) -> String {
        self.output.lock().unwrap_or_else(|e| e.into_inner()).text.clone()
    }
}

/// A readable reason for a failed push or fetch.
pub fn remote_error(work: Work, output: &str, code: Option<i32>) -> String {
    let has = |needle: &str| output.contains(needle);
    if has("has no upstream branch") {
        return "This branch has no upstream yet.".into();
    }
    if has("[rejected]") || has("non-fast-forward") || has("fetch first") {
        return "The remote has commits that you do not have. Fetch, merge or rebase them, then push again.".into();
    }
    if has("Permission denied") || has("Authentication failed") || has("could not read Username") || has("terminal prompts disabled") {
        return "git could not log in to the remote. Check your SSH key, SSH agent or credential helper.".into();
    }
    if has("does not appear to be a git repository") || has("No such remote") || has("No configured push destination") {
        return "git found no remote to talk to. Add one with git remote add.".into();
    }
    if has("Could not resolve host") || has("Network is unreachable") || has("Connection timed out") {
        return "The remote cannot be reached. Check the network.".into();
    }
    let line = output.lines().map(str::trim).rfind(|l| l.starts_with("error:") || l.starts_with("fatal:"));
    match (line, code) {
        (Some(l), _) => {
            format!("git {} failed: {}", work.label().to_lowercase(), l.trim_start_matches("error:").trim_start_matches("fatal:").trim())
        }
        (None, Some(c)) => format!("git {} stopped with exit code {c}.", work.label().to_lowercase()),
        (None, None) => format!("git {} was stopped.", work.label().to_lowercase()),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::ui::editor::review::tests::TempDir;

    /// A home of its own for git: an empty global configuration with a name
    /// and email, no system configuration, no credential helper of yours.
    pub fn sandbox_env(tmp: &Path) -> Vec<(OsString, OsString)> {
        let home = tmp.join("home");
        std::fs::create_dir_all(&home).unwrap();
        let config = home.join(".gitconfig");
        std::fs::write(&config, "[user]\n\tname = Test User\n\temail = test@example.org\n[init]\n\tdefaultBranch = main\n").unwrap();
        [
            ("HOME", home.as_os_str().to_owned()),
            ("XDG_CONFIG_HOME", home.join(".config").into_os_string()),
            ("GIT_CONFIG_GLOBAL", config.into_os_string()),
            ("GIT_CONFIG_NOSYSTEM", "1".into()),
        ]
        .into_iter()
        .map(|(k, v)| (OsString::from(k), v))
        .collect()
    }

    /// A plain git command with the test environment, for setting repositories up.
    pub fn sh(env: &[(OsString, OsString)], dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git").envs(env.iter().cloned()).arg("-C").arg(dir).args(args).output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// A user repository with one commit of `run.py` and `notes.md`.
    pub fn user_repo(tmp: &TempDir) -> (PathBuf, Vec<(OsString, OsString)>) {
        let env = sandbox_env(&tmp.0);
        let repo = tmp.0.join("project");
        std::fs::create_dir_all(&repo).unwrap();
        sh(&env, &repo, &["init", "-q", "-b", "main"]);
        let run = "import time\n\n\ndef run():\n    time.sleep(1)\n    return 1\n\n\ndef stop():\n    \"\"\"Stops the run.\n\n    Nothing to clean up yet, so it\n    only reports success to the\n    caller.\n    \"\"\"\n    return 0\n";
        std::fs::write(repo.join("run.py"), run).unwrap();
        std::fs::write(repo.join("notes.md"), "# Notes\n\none\ntwo\n").unwrap();
        sh(&env, &repo, &["add", "-A"]);
        sh(&env, &repo, &["commit", "-q", "-m", "first"]);
        (repo.canonicalize().unwrap(), env)
    }

    pub fn open_user(repo: &Path, env: &[(OsString, OsString)]) -> Git {
        open(repo, &Areas::default(), &Trust::default(), env).unwrap()
    }

    #[test]
    fn status_records_parse() {
        let z = "0".repeat(40);
        let (a, b, c) = ("a".repeat(40), "b".repeat(40), "c".repeat(40));
        let raw = format!(
            "# branch.oid {a}\0# branch.head main\0# branch.upstream origin/main\0# branch.ab +2 -1\0\
             1 M. N... 100644 100644 100644 {a} {b} src/lib.rs\0\
             1 .M N... 100644 100644 100644 {a} {a} has space.md\0\
             1 MM N... 100644 100644 100644 {a} {c} both.txt\0\
             1 A. N... 000000 100644 100644 {z} {b} new.rs\0\
             1 D. N... 100644 000000 000000 {a} {z} gone.rs\0\
             2 R. N... 100644 100644 100644 {a} {a} R100 renamed.rs\0old name.rs\0\
             u UU N... 100644 100644 100644 100644 {a} {b} {c} conflict.txt\0\
             ? untracked dir/file.txt\0! ignored.log\0"
        );
        let s = parse_status(raw.as_bytes());
        assert_eq!((s.branch.as_deref(), s.oid.as_deref(), s.upstream.as_deref()), (Some("main"), Some(a.as_str()), Some("origin/main")));
        assert_eq!((s.ahead, s.behind), (2, 1));
        let paths = |it: &mut dyn Iterator<Item = &FileStatus>| it.map(|f| f.path.clone()).collect::<Vec<_>>();
        assert_eq!(paths(&mut s.staged()), ["src/lib.rs", "both.txt", "new.rs", "gone.rs", "renamed.rs"]);
        assert_eq!(paths(&mut s.changed()), ["has space.md", "both.txt", "conflict.txt"]);
        assert_eq!(paths(&mut s.untracked()), ["untracked dir/file.txt"]);
        let new = s.file("new.rs").unwrap();
        assert_eq!((new.x, new.y, new.head.clone(), new.index.clone()), ('A', '.', None, Some(b.clone())));
        assert_eq!(s.file("gone.rs").unwrap().index, None, "zero ids are no blob");
        assert_eq!(s.file("conflict.txt").map(|f| (f.kind, f.head.clone())), Some((Kind::Conflict, Some(b.clone()))));
        assert!(s.file("old name.rs").is_none() && s.file("ignored.log").is_none());
        assert!(s.has_head());

        let fresh = parse_status(b"# branch.oid (initial)\0# branch.head main\0? a.txt\0");
        assert!(!fresh.has_head() && fresh.upstream.is_none() && fresh.ahead == 0);
        assert_eq!(parse_status(b"# branch.oid abc\0# branch.head (detached)\0").branch, None);
    }

    #[test]
    fn real_status_and_texts() {
        let tmp = TempDir::new("git-status");
        let (repo, env) = user_repo(&tmp);
        let git = open_user(&repo, &env);
        assert_eq!((git.mode, git.name()), (Mode::User, "project".to_string()));
        std::fs::write(repo.join("notes.md"), "# Notes\n\none\nthree\n").unwrap();
        std::fs::write(repo.join("new file.txt"), "hello\n").unwrap();
        sh(&env, &repo, &["add", "notes.md"]);
        std::fs::write(repo.join("notes.md"), "# Notes\n\none\nthree\nfour\n").unwrap();
        let s = git.status().unwrap();
        assert_eq!(s.branch.as_deref(), Some("main"));
        let notes = s.file("notes.md").unwrap();
        assert!(notes.staged() && notes.changed());
        assert_eq!(git.blob(notes.index.as_deref().unwrap()).unwrap(), b"# Notes\n\none\nthree\n");
        assert_eq!(git.blob(notes.head.as_deref().unwrap()).unwrap(), b"# Notes\n\none\ntwo\n");
        assert_eq!(git.disk("notes.md").unwrap().unwrap(), b"# Notes\n\none\nthree\nfour\n");
        assert_eq!(git.disk("missing.txt").unwrap(), None);
        assert!(s.file("new file.txt").unwrap().untracked());
        assert!(!repo.join(".git/index.lock").exists());
    }

    #[test]
    fn commit_and_amend() {
        let tmp = TempDir::new("git-commit");
        let (repo, env) = user_repo(&tmp);
        let git = open_user(&repo, &env);
        assert!(git.commit("   ", false, true).unwrap_err().contains("needs a message"));
        std::fs::write(repo.join("notes.md"), "# Notes\n\none\nthree\n").unwrap();
        git.stage("notes.md").unwrap();
        let id = git.commit("Change the notes\n\nTwo became three.", false, true).unwrap();
        assert_eq!(sh(&env, &repo, &["rev-parse", "--short", "HEAD"]), id);
        assert_eq!(sh(&env, &repo, &["log", "-1", "--format=%s|%an|%ae"]), "Change the notes|Test User|test@example.org");
        assert_eq!(sh(&env, &repo, &["log", "-1", "--format=%b"]), "Two became three.");
        assert_eq!(git.last_message().unwrap(), "Change the notes\n\nTwo became three.");
        assert!(git.status().unwrap().staged().next().is_none());

        let amended = git.commit("Change the notes, amended", true, true).unwrap();
        assert_ne!(amended, id);
        assert_eq!(sh(&env, &repo, &["rev-list", "--count", "HEAD"]), "2", "amend replaces the commit");
        assert_eq!(sh(&env, &repo, &["log", "-1", "--format=%s"]), "Change the notes, amended");
    }

    #[test]
    fn hooks_run_for_you_but_not_for_agents() {
        let tmp = TempDir::new("git-hooks");
        let (repo, env) = user_repo(&tmp);
        std::fs::create_dir_all(repo.join(".githooks")).unwrap();
        let hook = repo.join(".githooks/pre-commit");
        std::fs::write(&hook, format!("#!/bin/sh\necho ran >> {}\n", tmp.0.join("hook-ran").display())).unwrap();
        std::fs::set_permissions(&hook, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        sh(&env, &repo, &["config", "core.hooksPath", ".githooks"]);
        let git = open_user(&repo, &env);
        assert_eq!(git.hooks_in_tree().as_deref(), Some(".githooks"));
        std::fs::write(repo.join("a.txt"), "a\n").unwrap();
        git.stage("a.txt").unwrap();
        git.commit("yours", false, true).unwrap();
        assert!(tmp.0.join("hook-ran").exists(), "your commit runs your hooks");
        std::fs::remove_file(tmp.0.join("hook-ran")).unwrap();
        std::fs::write(repo.join("b.txt"), "b\n").unwrap();
        let reply = agent_act(&git, "stage", &serde_json::json!({ "path": "b.txt" })).unwrap();
        assert_eq!(reply, "staged b.txt");
        assert!(agent_act(&git, "commit", &serde_json::json!({ "message": "from the agent" })).unwrap().starts_with("committed "));
        assert!(!tmp.0.join("hook-ran").exists(), "an agent's commit runs no hook");
        assert!(agent_act(&git, "push", &serde_json::json!({})).unwrap_err().contains("stays with the user"));
        assert!(agent_act(&git, "stage", &serde_json::json!({ "path": "../outside" })).is_err());
        assert!(agent_act(&git, "stage", &serde_json::json!({ "path": "/etc/passwd" })).is_err());
    }

    #[test]
    fn repositories_agents_could_write_are_refused() {
        let tmp = TempDir::new("git-trust");
        let env = sandbox_env(&tmp.0);
        let (agent, ws) = (tmp.0.join("data/agents/wren"), tmp.0.join("code"));
        for dir in [agent.clone(), ws.join("lib"), ws.join("deep/inner")] {
            std::fs::create_dir_all(&dir).unwrap();
            sh(&env, &dir, &["init", "-q"]);
        }
        sh(&env, &ws, &["init", "-q"]);
        let areas = Areas { agents: vec![canonical(&agent)], workspaces: vec![canonical(&ws)] };
        assert_eq!(open(&agent, &areas, &Trust::default(), &env).unwrap().mode, Mode::Agent);
        assert_eq!(open(&ws, &areas, &Trust::default(), &env).unwrap().mode, Mode::User, "the folder's own .git is read-only for agents");
        assert_eq!(open(&ws.join("lib"), &areas, &Trust::default(), &env).unwrap().mode, Mode::User, "one level down is protected too");
        let err = open(&ws.join("deep/inner"), &areas, &Trust::default(), &env).unwrap_err();
        assert!(err.contains("deeper than the sandbox protects"), "{err}");
        std::fs::create_dir_all(agent.join("sub")).unwrap();
        sh(&env, &agent.join("sub"), &["init", "-q"]);
        assert!(open(&agent.join("sub"), &areas, &Trust::default(), &env).unwrap_err().contains("inside an agent's directory"));
        let outside = tmp.0.join("elsewhere");
        std::fs::create_dir_all(&outside).unwrap();
        assert!(open(&outside, &areas, &Trust::default(), &env).unwrap_err().contains("not a git repository"));
    }

    /// Waits until a push or fetch ended and returns its result.
    pub fn finish(mut remote: Remote) -> Result<(), String> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while remote.running() {
            assert!(std::time::Instant::now() < deadline, "git did not finish: {}", remote.text());
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        remote.result.take().unwrap()
    }

    #[test]
    fn push_to_a_bare_remote_with_and_without_upstream() {
        let tmp = TempDir::new("git-push");
        let (repo, env) = user_repo(&tmp);
        let remote = tmp.0.join("remote.git");
        sh(&env, &tmp.0, &["init", "-q", "--bare", remote.to_str().unwrap()]);
        sh(&env, &repo, &["remote", "add", "origin", remote.to_str().unwrap()]);
        let git = open_user(&repo, &env);
        assert_eq!(git.remotes(), ["origin"]);
        assert_eq!(git.status().unwrap().upstream, None);

        let err = finish(git.push(None).unwrap()).unwrap_err();
        assert_eq!(err, "This branch has no upstream yet.");
        let pushed = git.push(Some(("origin", "main"))).unwrap();
        finish(pushed).unwrap();
        let s = git.status().unwrap();
        assert_eq!((s.upstream.as_deref(), s.ahead, s.behind), (Some("origin/main"), 0, 0));
        assert_eq!(sh(&env, &remote, &["rev-parse", "main"]), sh(&env, &repo, &["rev-parse", "HEAD"]));

        std::fs::write(repo.join("notes.md"), "# Notes\n\nthree\n").unwrap();
        git.stage("notes.md").unwrap();
        git.commit("Three", false, true).unwrap();
        assert_eq!(git.status().unwrap().ahead, 1);
        let push = git.push(None).unwrap();
        assert_eq!(push.work, Work::Push);
        finish(push).unwrap();
        assert_eq!(git.status().unwrap().ahead, 0, "ahead and behind follow the push");
        assert_eq!(sh(&env, &remote, &["log", "-1", "--format=%s", "main"]), "Three");
        finish(git.fetch().unwrap()).unwrap();
    }

    #[test]
    fn progress_and_remote_errors() {
        let mut p = Progress::default();
        p.push("Counting objects:  50% (1/2)\rCounting objects: 100% (2/2), done.\nWriting");
        p.push(" objects: 10%\r");
        assert_eq!(p.text, "Counting objects: 100% (2/2), done.\nWriting objects: 10%");
        p.push("Writing objects: 100%\n");
        assert_eq!(p.text, "Counting objects: 100% (2/2), done.\nWriting objects: 100%\n");
        let rejected = " ! [rejected]        main -> main (fetch first)\nerror: failed to push some refs";
        assert!(remote_error(Work::Push, rejected, Some(1)).contains("Fetch, merge or rebase"));
        assert!(remote_error(Work::Push, "fatal: The current branch x has no upstream branch.", Some(128)).contains("no upstream"));
        assert_eq!(remote_error(Work::Fetch, "fatal: something odd", Some(128)), "git fetch failed: something odd");
        assert_eq!(remote_error(Work::Push, "", Some(3)), "git push stopped with exit code 3.");
        assert_eq!(relative_path("src/a.rs"), Ok("src/a.rs".into()));
        assert!(relative_path("").is_err() && relative_path("a/../../b").is_err());
    }
}
