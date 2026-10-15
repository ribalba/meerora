//! Per-agent sandboxes with bubblewrap (D16, closes Q16).
//!
//! Every agent run starts its CLI (claude, codex) inside `bwrap`, and the
//! Ollama runtime starts each tool command inside it. The sandbox shows:
//! - the system read-only: `/usr`, `/etc`, `/lib*`, `/bin`, `/sbin` and `/opt`
//!   when present, plus the resolver file that `/etc/resolv.conf` points to
//! - the runtime binary and the MCP bridge read-only, plus
//!   [`Isolation::read`](crate::Isolation::read)
//! - the agent directory read-write (D21), which is the working directory
//! - the agent's workspace folders read-write, with every `.git` in them
//!   read-only, so host-side git never runs a config the agent planted
//! - the CLI home read-write with the shared credentials file (Q15), and the
//!   CLI configuration that meerora writes read-only on top of it
//! - the daemon socket, so the meerora tools keep working
//! - a private `/tmp`, `/dev` and `/proc`, and an empty `$HOME`
//! - the network, because the CLIs talk to their API. Tool-level `web`
//!   gating stays as it is (D29).
//!
//! The sandbox runs in a new session, so it cannot push input into the
//! user's terminal, with its own pid and ipc namespaces, and it dies with
//! the daemon. A missing or failing bwrap fails the run with a readable
//! error. A run never falls back to running unsandboxed.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use meerora_protocol::workspace;

use crate::process::{self, ChildSpec};
use crate::{Isolation, McpBridge};

/// How to switch the sandbox off, for error messages.
pub const SWITCH_OFF: &str = "switch the sandbox off with `[sandbox] enabled = false` in meerora.toml";

/// System paths in mount order. `/usr` comes first, because `/bin` and
/// `/lib` are often symlinks into it.
pub const SYSTEM: &[&str] = &["/usr", "/etc", "/opt", "/bin", "/sbin", "/lib", "/lib32", "/lib64"];

/// Entries of the agent directory that the agent may read but not change.
/// `agent.toml` holds its runtime, role and capabilities (D8), `.git` is
/// committed by the daemon on the host, `state.json` and `logs/` are the
/// daemon's records of budget, sessions, chat and runs.
pub(crate) const PROTECTED: [&str; 4] = [".git", "agent.toml", "state.json", "logs"];

/// What one sandbox shows besides the system.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Mounts {
    /// The agent directory, read-write and the working directory.
    pub dir: PathBuf,
    /// Read-only. Missing paths are skipped.
    pub read: Vec<PathBuf>,
    /// The CLI home, read-write.
    pub write: Vec<PathBuf>,
    /// The shared credentials, read-write files mounted at another path, as
    /// `(source, destination)`.
    pub files: Vec<(PathBuf, PathBuf)>,
    /// The CLI configuration that meerora writes, read-only on top of a
    /// writable mount.
    pub read_only: Vec<PathBuf>,
    /// Unix sockets. Missing ones are skipped.
    pub sockets: Vec<PathBuf>,
    /// The user's home, which the sandbox replaces with an empty tmpfs.
    pub hide: Option<PathBuf>,
    /// Workspace folders, read-write with every `.git` in them read-only
    /// (see `meerora_protocol::workspace`).
    pub workspace: Vec<PathBuf>,
}

impl Mounts {
    /// The mounts of an agent run: its directory, the paths and sockets of
    /// `isolation` and the MCP bridge binary. The user's home is hidden.
    pub fn for_run(isolation: &Isolation, dir: &Path, mcp: Option<&McpBridge>) -> Self {
        let mut read = isolation.read.clone();
        if let Some(bridge) = mcp {
            read.push(bridge.command.clone());
        }
        Self {
            dir: dir.to_path_buf(),
            read,
            sockets: isolation.sockets.clone(),
            hide: user_home(),
            workspace: isolation.workspace.clone(),
            ..Self::default()
        }
    }
}

/// Variables a sandboxed child keeps from the daemon's environment. All
/// others are dropped, because the environment can carry tokens of other
/// programs that an agent with a shell could read.
const KEPT_VARS: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "LANG",
    "LANGUAGE",
    "TERM",
    "COLORTERM",
    "TZ",
    "NO_COLOR",
    "http_proxy",
    "https_proxy",
    "no_proxy",
    "all_proxy",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "NO_PROXY",
    "ALL_PROXY",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
];

/// Login and provider variables of the runtime CLIs. Only the CLI itself
/// gets them, tool commands do not.
const CREDENTIAL_PREFIXES: &[&str] = &["ANTHROPIC_", "OPENAI_", "AWS_", "CLOUD_ML_"];

/// The part of the daemon's environment a sandboxed child keeps. With
/// `credentials` it also keeps the login variables of the runtime CLIs,
/// including the claude ones in `claude::KEPT_ENV`.
pub fn kept_env(credentials: bool) -> Vec<(OsString, OsString)> {
    std::env::vars_os().filter(|(key, _)| keep_var(&key.to_string_lossy(), credentials)).collect()
}

fn keep_var(key: &str, credentials: bool) -> bool {
    KEPT_VARS.contains(&key)
        || key.starts_with("LC_")
        || (credentials
            && (CREDENTIAL_PREFIXES.iter().any(|p| key.starts_with(p))
                || crate::claude::KEPT_ENV.contains(&key)
                || key == "GOOGLE_APPLICATION_CREDENTIALS"))
}

/// `$HOME`, unless it is unset, relative or `/`.
pub fn user_home() -> Option<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME")?);
    (home.is_absolute() && home != Path::new("/")).then_some(home)
}

/// The bwrap binary, from PATH or `/usr/bin`.
pub fn find() -> Result<PathBuf, String> {
    process::find_binary("bwrap", None).or_else(|_| process::find_binary("bwrap", Some(Path::new("/usr/bin/bwrap")))).map_err(|_| {
        format!("The sandbox needs bubblewrap (bwrap), which is not installed. Install it, or {SWITCH_OFF}. Agents never run unsandboxed while it is on.")
    })
}

/// The arguments for bwrap that run `program` with `args` inside the sandbox.
pub fn args(mounts: &Mounts, program: &Path, args: &[OsString]) -> Vec<OsString> {
    let mut out: Vec<OsString> = ["--die-with-parent", "--new-session", "--unshare-pid", "--unshare-ipc"].map(OsString::from).to_vec();
    let mut push = |items: &[&OsStr]| out.extend(items.iter().map(|i| i.to_os_string()));
    let os = |s: &'static str| OsStr::new(s);
    for path in SYSTEM.iter().map(Path::new) {
        match std::fs::symlink_metadata(path) {
            Ok(meta) if meta.file_type().is_symlink() => {
                if let Ok(target) = std::fs::read_link(path) {
                    push(&[os("--symlink"), target.as_os_str(), path.as_os_str()]);
                }
            }
            Ok(meta) if meta.is_dir() => push(&[os("--ro-bind"), path.as_os_str(), path.as_os_str()]),
            _ => {}
        }
    }
    // /etc/resolv.conf is often a symlink into /run, which is not mounted.
    if let Ok(resolv) = std::fs::canonicalize("/etc/resolv.conf")
        && !resolv.starts_with("/etc")
        && !resolv.starts_with("/usr")
    {
        push(&[os("--ro-bind-try"), resolv.as_os_str(), resolv.as_os_str()]);
    }
    push(&[os("--proc"), os("/proc"), os("--dev"), os("/dev"), os("--tmpfs"), os("/tmp")]);
    if let Some(home) = &mounts.hide {
        push(&[os("--tmpfs"), home.as_os_str()]);
    }
    for path in mounts.read.iter().filter_map(|p| expand(p, mounts.hide.as_deref())) {
        push(&[os("--ro-bind-try"), path.as_os_str(), path.as_os_str()]);
    }
    for path in &mounts.write {
        push(&[os("--bind"), path.as_os_str(), path.as_os_str()]);
    }
    // Workspace folders are read-write, but every .git in them stays
    // read-only like PROTECTED below, because the user's git reads it on the
    // host. A nested repository is bound onto itself first, which makes it a
    // mount point, so the agent cannot move it away and put a repository of
    // its own with a planted config in its place.
    for folder in &mounts.workspace {
        push(&[os("--bind"), folder.as_os_str(), folder.as_os_str()]);
        let nested = workspace::nested_repos(folder);
        for repo in &nested {
            push(&[os("--bind"), repo.as_os_str(), repo.as_os_str()]);
        }
        for git in std::iter::once(folder.join(".git")).chain(nested.iter().map(|r| r.join(".git"))) {
            push(&[os("--ro-bind-try"), git.as_os_str(), git.as_os_str()]);
        }
    }
    push(&[os("--bind"), mounts.dir.as_os_str(), mounts.dir.as_os_str()]);
    // What meerora itself reads from the agent directory stays read-only for
    // the agent: its git data (host-side git would run commands from a planted
    // config), its settings and capabilities, the daemon's state and its logs.
    for name in PROTECTED {
        let path = mounts.dir.join(name);
        push(&[os("--ro-bind-try"), path.as_os_str(), path.as_os_str()]);
    }
    for (source, dest) in &mounts.files {
        push(&[os("--bind"), source.as_os_str(), dest.as_os_str()]);
    }
    for path in &mounts.read_only {
        push(&[os("--ro-bind"), path.as_os_str(), path.as_os_str()]);
    }
    for path in &mounts.sockets {
        push(&[os("--bind-try"), path.as_os_str(), path.as_os_str()]);
    }
    push(&[os("--chdir"), mounts.dir.as_os_str(), os("--"), program.as_os_str()]);
    out.extend(args.iter().cloned());
    out
}

/// An absolute path, with a leading `~/` taken as the user's home. Relative
/// paths are skipped, because bwrap cannot mount them.
fn expand(path: &Path, home: Option<&Path>) -> Option<PathBuf> {
    if let (Ok(rest), Some(home)) = (path.strip_prefix("~"), home) {
        return Some(home.join(rest));
    }
    if path.is_absolute() {
        Some(path.to_path_buf())
    } else {
        tracing::warn!("{} is not an absolute path, so the sandbox does not show it", path.display());
        None
    }
}

/// Wraps a child in the sandbox. bwrap becomes the program, and the real
/// program runs from its canonical path, which the sandbox shows read-only.
pub(crate) fn wrap(mut spec: ChildSpec, mut mounts: Mounts) -> Result<ChildSpec, String> {
    let bwrap = find()?;
    let program = spec.program.canonicalize().map_err(|e| format!("{} ({}): {e}", spec.label, spec.program.display()))?;
    mounts.read.push(program.clone());
    spec.args = args(&mounts, &program, &spec.args);
    spec.program = bwrap;
    spec.sandboxed = true;
    Ok(spec)
}

/// A readable error when bwrap itself failed, from the child's stderr.
pub fn explain(stderr: &str) -> Option<String> {
    let line = stderr.lines().map(str::trim).find(|l| l.starts_with("bwrap:"))?;
    Some(format!("The sandbox could not start the run ({line}). If bubblewrap cannot work on this system, {SWITCH_OFF}."))
}

/// Runs `true` inside a sandbox with `mounts`, to find out whether bwrap works here.
pub(crate) fn probe(bwrap: &Path, mounts: &Mounts) -> Result<(), String> {
    let output = Command::new(bwrap)
        .args(args(mounts, Path::new("sh"), &["-c".into(), "true".into()]))
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("cannot start {}: {e}. If bubblewrap cannot work on this system, {SWITCH_OFF}.", bwrap.display()))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(explain(&stderr).unwrap_or_else(|| {
        format!(
            "The sandbox {} ({}). If bubblewrap cannot work on this system, {SWITCH_OFF}.",
            process::describe_exit(output.status),
            stderr.trim()
        )
    }))
}

/// What `meerora-agentd check` says about the sandbox. `Ok` holds the
/// bubblewrap version once a test command ran inside it, `Err` says why it failed.
pub fn check() -> Result<String, String> {
    let bwrap = find()?;
    let version = Command::new(&bwrap)
        .arg("--version")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "bubblewrap".into());
    probe(&bwrap, &Mounts { dir: std::env::temp_dir(), hide: user_home(), ..Mounts::default() })?;
    Ok(format!("{version} works ({})", bwrap.display()))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn only_allowed_variables_enter_the_sandbox() {
        for key in ["PATH", "HOME", "LANG", "LC_TIME", "HTTPS_PROXY"] {
            assert!(keep_var(key, false), "{key}");
        }
        for key in ["CLAUDE_CODE_MESSAGING_TOKEN", "GITHUB_TOKEN", "SSH_AUTH_SOCK", "DBUS_SESSION_BUS_ADDRESS", "ANTHROPIC_API_KEY"] {
            assert!(!keep_var(key, false), "{key} reaches a tool command");
        }
        for key in ["ANTHROPIC_API_KEY", "OPENAI_API_KEY", "CLAUDE_CODE_OAUTH_TOKEN", "CLAUDE_CONFIG_DIR"] {
            assert!(keep_var(key, true), "the CLI needs {key}");
        }
        assert!(!keep_var("CLAUDE_CODE_MESSAGING_TOKEN", true));
        assert!(!keep_var("GITHUB_TOKEN", true));
    }

    fn strings(args: &[OsString]) -> Vec<String> {
        args.iter().map(|a| a.to_string_lossy().into_owned()).collect()
    }

    fn position(args: &[String], window: &[&str]) -> usize {
        args.windows(window.len()).position(|w| w == window).unwrap_or_else(|| panic!("{window:?} missing in {args:?}"))
    }

    /// A temporary directory that is removed when dropped.
    pub(crate) struct Scratch(pub PathBuf);

    impl Scratch {
        pub fn new(name: &str) -> Self {
            use std::sync::atomic::{AtomicU32, Ordering};
            static N: AtomicU32 = AtomicU32::new(0);
            let dir = std::env::temp_dir().join(format!("meerora-sbx-{name}-{}-{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst)));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// bwrap, if it works here. Prints why a test is skipped otherwise.
    pub(crate) fn working_bwrap(test: &str) -> Option<PathBuf> {
        match check() {
            Ok(_) => find().ok(),
            Err(e) => {
                eprintln!("note: {test} is skipped, because the sandbox does not work here: {e}");
                None
            }
        }
    }

    #[test]
    fn command_line() {
        let mounts = Mounts {
            dir: "/data/agents/wren".into(),
            read: vec!["/opt/meerora/meerora-mcp".into(), "~/notes".into(), "relative/path".into()],
            write: vec!["/data/runtimes/claude/wren".into()],
            files: vec![("/home/u/.claude/.credentials.json".into(), "/data/runtimes/claude/wren/.credentials.json".into())],
            read_only: vec!["/data/runtimes/codex/wren/config.toml".into()],
            sockets: vec!["/run/user/1000/meerora/agentd.sock".into()],
            hide: Some("/home/u".into()),
            workspace: Vec::new(),
        };
        let a = strings(&args(&mounts, Path::new("/usr/bin/claude"), &["-p".into(), "--verbose".into()]));
        assert_eq!(a[..4], ["--die-with-parent", "--new-session", "--unshare-pid", "--unshare-ipc"]);
        assert!(!a.contains(&"--unshare-net".to_string()), "the CLIs need the network");
        let usr = position(&a, &["--ro-bind", "/usr", "/usr"]);
        let tmp = position(&a, &["--tmpfs", "/tmp"]);
        let home = position(&a, &["--tmpfs", "/home/u"]);
        position(&a, &["--proc", "/proc"]);
        position(&a, &["--dev", "/dev"]);
        let bridge = position(&a, &["--ro-bind-try", "/opt/meerora/meerora-mcp", "/opt/meerora/meerora-mcp"]);
        position(&a, &["--ro-bind-try", "/home/u/notes", "/home/u/notes"]);
        assert!(!a.iter().any(|x| x == "relative/path"), "relative paths cannot be mounted");
        let cli_home = position(&a, &["--bind", "/data/runtimes/claude/wren", "/data/runtimes/claude/wren"]);
        let dir = position(&a, &["--bind", "/data/agents/wren", "/data/agents/wren"]);
        let creds = position(&a, &["--bind", "/home/u/.claude/.credentials.json", "/data/runtimes/claude/wren/.credentials.json"]);
        let config = position(&a, &["--ro-bind", "/data/runtimes/codex/wren/config.toml", "/data/runtimes/codex/wren/config.toml"]);
        position(&a, &["--bind-try", "/run/user/1000/meerora/agentd.sock", "/run/user/1000/meerora/agentd.sock"]);
        // Later mounts cover earlier ones, so the order matters.
        assert!(usr < tmp && tmp < home && home < bridge && bridge < cli_home && cli_home < creds && cli_home < config && home < dir);
        let n = a.len();
        assert_eq!(a[n - 6..], ["--chdir", "/data/agents/wren", "--", "/usr/bin/claude", "-p", "--verbose"]);
    }

    #[test]
    fn bwrap_errors_are_explained() {
        let text = explain("warning\nbwrap: setting up uid map: Permission denied\n").unwrap();
        assert!(text.contains("setting up uid map") && text.contains("[sandbox] enabled = false"), "{text}");
        assert_eq!(explain("claude: not logged in"), None);
    }

    /// The sandbox shows the agent directory and hides the rest of the home.
    #[test]
    fn the_home_stays_hidden() {
        let Some(bwrap) = working_bwrap("the_home_stays_hidden") else { return };
        let scratch = Scratch::new("home");
        let home = scratch.0.join("home");
        let agent = home.join(".local/share/meerora/agents/wren");
        std::fs::create_dir_all(&agent).unwrap();
        std::fs::create_dir_all(home.join("Documents")).unwrap();
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        std::fs::write(agent.join("notes.md"), "my notes").unwrap();
        std::fs::write(home.join("Documents/secret.txt"), "s3cret").unwrap();
        std::fs::write(home.join(".ssh/id_ed25519"), "private key").unwrap();
        std::fs::write(scratch.0.join("next-to-home.txt"), "also secret").unwrap();

        let mounts = Mounts { dir: agent.clone(), hide: Some(home.clone()), ..Mounts::default() };
        let run = |script: String| {
            let out = Command::new(&bwrap)
                .args(args(&mounts, Path::new("/bin/sh"), &["-c".into(), script.into()]))
                .stdin(Stdio::null())
                .output()
                .unwrap();
            (out.status.success(), String::from_utf8_lossy(&out.stdout).into_owned(), String::from_utf8_lossy(&out.stderr).into_owned())
        };
        let (ok, out, _) = run(format!("cat {}/notes.md && pwd && echo made > {}/made.txt", agent.display(), agent.display()));
        assert!(ok && out.contains("my notes") && out.contains(&agent.display().to_string()), "{out}");
        assert_eq!(std::fs::read_to_string(agent.join("made.txt")).unwrap().trim(), "made", "the agent directory is writable");

        for secret in [home.join("Documents/secret.txt"), home.join(".ssh/id_ed25519"), scratch.0.join("next-to-home.txt")] {
            let (ok, out, err) = run(format!("cat {}", secret.display()));
            assert!(!ok && !out.contains("secret") && !out.contains("key"), "{} was readable: {out}", secret.display());
            assert!(err.contains("No such file"), "{err}");
        }
        let (_, out, _) = run(format!("ls -A {}", home.display()));
        assert_eq!(out.trim(), ".local", "the home only holds the path to the agent directory");
        // Writes outside the agent directory stay in the sandbox.
        run(format!("echo x > {}/dropped.txt; echo x > /tmp/dropped.txt", home.display()));
        assert!(!home.join("dropped.txt").exists());
    }

    /// An agent cannot change what meerora reads from its directory: its
    /// settings and capabilities, its git data (the daemon runs git on the
    /// host) and the daemon's logs. Everything else stays writable.
    #[test]
    fn meerora_files_stay_read_only() {
        let Some(bwrap) = working_bwrap("meerora_files_stay_read_only") else { return };
        let scratch = Scratch::new("protected");
        let agent = scratch.0.join("agents/wren");
        std::fs::create_dir_all(agent.join(".git")).unwrap();
        std::fs::create_dir_all(agent.join("logs")).unwrap();
        std::fs::create_dir_all(agent.join("memory")).unwrap();
        std::fs::write(agent.join("agent.toml"), "name = \"Wren\"\n").unwrap();
        std::fs::write(agent.join(".git/config"), "[core]\n").unwrap();
        std::fs::write(agent.join("logs/messages.jsonl"), "").unwrap();
        let mounts = Mounts { dir: agent.clone(), ..Mounts::default() };
        let run = |script: &str| {
            Command::new(&bwrap)
                .args(args(&mounts, Path::new("/bin/sh"), &["-c".into(), script.into()]))
                .stdin(Stdio::null())
                .output()
                .unwrap()
                .status
                .success()
        };
        let d = agent.display();
        assert!(!run(&format!("echo 'granted = [\"shell\"]' >> {d}/agent.toml")));
        assert!(!run(&format!("printf '[core]\\n fsmonitor = evil\\n' >> {d}/.git/config")));
        assert!(!run(&format!("echo forged >> {d}/logs/messages.jsonl")));
        assert!(!run(&format!("mv {d}/agent.toml {d}/old.toml")), "a protected file cannot be moved away either");
        assert!(!run(&format!("rm -rf {d}/.git")), "nor removed");
        assert_eq!(std::fs::read_to_string(agent.join("agent.toml")).unwrap(), "name = \"Wren\"\n");
        assert_eq!(std::fs::read_to_string(agent.join(".git/config")).unwrap(), "[core]\n");
        assert!(run(&format!("cat {d}/agent.toml && echo note > {d}/memory/note.md")), "reading and the rest stay allowed");
        assert_eq!(std::fs::read_to_string(agent.join("memory/note.md")).unwrap().trim(), "note");
    }

    /// claude saves its credentials with a rename and writes in place when the
    /// rename fails with EBUSY. Over a bind mount the rename fails, and the
    /// in-place write reaches the shared file (homes.rs relies on this).
    #[test]
    fn a_mounted_file_is_written_in_place() {
        let Some(bwrap) = working_bwrap("a_mounted_file_is_written_in_place") else { return };
        let scratch = Scratch::new("cred");
        let (agent, cli, shared) = (scratch.0.join("agent"), scratch.0.join("cli"), scratch.0.join("user/.credentials.json"));
        for dir in [&agent, &cli, &shared.parent().unwrap().to_path_buf()] {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::fs::write(&shared, "old").unwrap();
        std::fs::write(cli.join(".credentials.json"), "").unwrap();
        let inode = |p: &Path| std::os::unix::fs::MetadataExt::ino(&std::fs::metadata(p).unwrap());
        let before = inode(&shared);
        let mounts = Mounts {
            dir: agent,
            write: vec![cli.clone()],
            files: vec![(shared.clone(), cli.join(".credentials.json"))],
            ..Mounts::default()
        };
        let target = cli.join(".credentials.json");
        let script = format!(
            "echo new > {tmp} && if mv -f {tmp} {target} 2>/dev/null; then echo renamed; else echo busy; echo new > {target}; fi",
            tmp = cli.join(".credentials.json.tmp").display(),
            target = target.display()
        );
        let out = Command::new(&bwrap).args(args(&mounts, Path::new("/bin/sh"), &["-c".into(), script.into()])).output().unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(out.status.success() && stdout.contains("busy"), "{stdout} {}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(std::fs::read_to_string(&shared).unwrap().trim(), "new");
        assert_eq!(inode(&shared), before, "still the same file");
    }

    /// Workspace folders are read-write, their own `.git` and those of
    /// nested repositories read-only, after the hidden home and before the
    /// agent directory.
    #[test]
    fn workspace_folders_mount_with_read_only_git() {
        let scratch = Scratch::new("ws-args");
        let ws = scratch.0.join("home/code/project");
        std::fs::create_dir_all(ws.join(".git")).unwrap();
        std::fs::create_dir_all(ws.join("vendor/lib/.git")).unwrap();
        std::fs::create_dir_all(ws.join("plugin/.git")).unwrap();
        std::fs::create_dir_all(ws.join("src")).unwrap();
        let mounts = Mounts {
            dir: "/data/agents/wren".into(),
            hide: Some(scratch.0.join("home")),
            workspace: vec![ws.clone()],
            ..Mounts::default()
        };
        let a = strings(&args(&mounts, Path::new("/usr/bin/claude"), &[]));
        let s = |p: &Path| p.display().to_string();
        let home = position(&a, &["--tmpfs", &s(&scratch.0.join("home"))]);
        let folder = position(&a, &["--bind", &s(&ws), &s(&ws)]);
        let plugin = position(&a, &["--bind", &s(&ws.join("plugin")), &s(&ws.join("plugin"))]);
        let own_git = position(&a, &["--ro-bind-try", &s(&ws.join(".git")), &s(&ws.join(".git"))]);
        let plugin_git = position(&a, &["--ro-bind-try", &s(&ws.join("plugin/.git")), &s(&ws.join("plugin/.git"))]);
        let dir = position(&a, &["--bind", "/data/agents/wren", "/data/agents/wren"]);
        assert!(home < folder && folder < plugin && plugin < own_git && plugin < plugin_git && own_git < dir, "{a:?}");
        assert!(!a.iter().any(|x| x.contains("vendor/lib")), "only one level deep: {a:?}");
        assert!(!a.iter().any(|x| x.ends_with("/src")), "plain folders need no mount of their own");
        let n = a.len();
        assert_eq!(a[n - 4..], ["--chdir", "/data/agents/wren", "--", "/usr/bin/claude"], "runs still start in the agent directory");
    }

    /// The agent edits code in its workspace, but neither the `.git` of the
    /// folder nor that of a nested repository, and it cannot swap a nested
    /// repository for one of its own.
    #[test]
    fn the_agent_edits_a_workspace_but_not_its_git() {
        let Some(bwrap) = working_bwrap("the_agent_edits_a_workspace_but_not_its_git") else { return };
        let scratch = Scratch::new("ws-real");
        let (agent, ws) = (scratch.0.join("agents/wren"), scratch.0.join("code/project"));
        for dir in [agent.clone(), ws.join(".git/hooks"), ws.join("lib/.git"), ws.join("src")] {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::fs::write(ws.join(".git/config"), "[core]\n").unwrap();
        std::fs::write(ws.join("lib/.git/config"), "[core]\n").unwrap();
        std::fs::write(ws.join("src/main.rs"), "fn main() {}\n").unwrap();
        let mounts = Mounts { dir: agent.clone(), workspace: vec![ws.clone()], ..Mounts::default() };
        let run = |script: String| {
            Command::new(&bwrap)
                .args(args(&mounts, Path::new("/bin/sh"), &["-c".into(), script.into()]))
                .stdin(Stdio::null())
                .output()
                .unwrap()
                .status
                .success()
        };
        let w = ws.display();
        assert!(run(format!("echo '// edited' >> {w}/src/main.rs && echo new > {w}/src/new.rs && echo l > {w}/lib/x.txt")));
        assert_eq!(std::fs::read_to_string(ws.join("src/main.rs")).unwrap(), "fn main() {}\n// edited\n");
        assert!(ws.join("src/new.rs").is_file() && ws.join("lib/x.txt").is_file());
        assert!(!run(format!("printf '[core]\\n fsmonitor = evil\\n' >> {w}/.git/config")), "the folder's git config");
        assert!(!run(format!("echo evil > {w}/.git/hooks/pre-commit")), "its hooks");
        assert!(!run(format!("rm -rf {w}/.git")), "its .git cannot go");
        assert!(!run(format!("mv {w}/.git {w}/old-git")), "nor move");
        assert!(!run(format!("echo x >> {w}/lib/.git/config")), "a nested repository's config");
        assert!(!run(format!("mv {w}/lib {w}/lib2")), "a nested repository cannot be moved away");
        assert!(!run(format!("rm -rf {w}/lib")));
        assert_eq!(std::fs::read_to_string(ws.join(".git/config")).unwrap(), "[core]\n");
        assert_eq!(std::fs::read_to_string(ws.join("lib/.git/config")).unwrap(), "[core]\n");
        assert!(!ws.join(".git/hooks/pre-commit").exists() && ws.join("lib/.git").is_dir());
        assert!(run(format!("cat {w}/.git/config > /dev/null")), "reading git data stays allowed");
    }
}
