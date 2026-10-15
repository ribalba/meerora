//! Own homes for the claude and codex CLIs (closes Q15).
//!
//! Each agent gets `<data dir>/runtimes/<cli>/<agent>/` as `CLAUDE_CONFIG_DIR`
//! or `CODEX_HOME`. The home lies outside the agent directory, so an agent
//! cannot rewrite its own CLI configuration, and it holds none of the user's
//! configuration, memory files, rules, skills or MCP servers (D29).
//!
//! The user's login is shared, never copied, because refresh tokens rotate
//! and a copy would log one side out. How the CLIs save a refreshed token
//! decides how it is shared (checked on 14 Oct 2026):
//! - codex 0.162 rewrites `auth.json` in place (`open` with `O_TRUNC`), which
//!   follows a symlink. The home holds a symlink to the user's `auth.json`,
//!   and the sandbox mounts that file at its own path, so the link resolves.
//! - claude 2.1.299 saves `.credentials.json` through a temporary file and a
//!   rename, and it refuses a symlink there (`O_NOFOLLOW`). When the rename
//!   fails with EBUSY it writes the file in place. So the sandbox mounts the
//!   user's file over `<home>/.credentials.json`. The rename then hits the
//!   mount point, fails with EBUSY, and claude writes the shared file in place.
//!   Without the sandbox a shared file cannot be kept, so such a run needs a
//!   login of its own (`CLAUDE_CONFIG_DIR=<home> claude auth login`).
//!
//! A home that holds a login of its own (a regular file with content) keeps it.

use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use crate::Isolation;
use crate::sandbox::Mounts;

pub(crate) const CLAUDE_CREDENTIALS: &str = ".credentials.json";
pub(crate) const CODEX_AUTH: &str = "auth.json";

/// The configuration of every codex home. Per-run settings come as `-c`
/// options on top (see codex.rs).
pub(crate) const CODEX_CONFIG: &str = "\
# meerora writes this file before every run of this agent, and the sandbox
# shows it read-only. The user's own codex configuration, rules, memories and
# AGENTS.md do not apply here (Q15).
cli_auth_credentials_store = \"file\"
check_for_update_on_startup = false
";

/// A prepared CLI home and what the sandbox has to show of it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Home {
    pub dir: PathBuf,
    /// Shared credentials, mounted read-write at `(source, destination)`.
    pub files: Vec<(PathBuf, PathBuf)>,
    /// Files in the home that the sandbox shows read-only.
    pub read_only: Vec<PathBuf>,
}

impl Home {
    /// Adds the home to the mounts of a sandbox.
    pub fn mount(self, mounts: &mut Mounts) {
        mounts.write.push(self.dir);
        mounts.files.extend(self.files);
        mounts.read_only.extend(self.read_only);
    }
}

/// `<runtimes>/<cli>/<agent>/`.
pub(crate) fn dir(isolation: &Isolation, cli: &str, agent: &str) -> PathBuf {
    isolation.runtimes.join(cli).join(agent)
}

/// The user's own claude directory, `$CLAUDE_CONFIG_DIR` or else `~/.claude`.
pub(crate) fn user_claude_dir() -> Option<PathBuf> {
    env_dir("CLAUDE_CONFIG_DIR", ".claude")
}

/// The user's own codex directory, `$CODEX_HOME` or else `~/.codex`.
pub(crate) fn user_codex_dir() -> Option<PathBuf> {
    env_dir("CODEX_HOME", ".codex")
}

fn env_dir(var: &str, below_home: &str) -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os(var).filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    std::env::var_os("HOME").map(|home| Path::new(&home).join(below_home))
}

fn create(dir: &Path) -> Result<(), String> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|e| format!("cannot create the CLI home {}: {e}", dir.display()))
}

/// A regular file with content, not a symlink, is a login of the home itself.
fn own_login(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.is_file() && m.len() > 0)
}

/// The user's credentials file with symlinks resolved, when it exists.
fn user_file(dir: Option<&Path>, name: &str) -> Option<PathBuf> {
    let path = dir?.join(name).canonicalize().ok()?;
    path.is_file().then_some(path)
}

/// Prepares `dir` as `CLAUDE_CONFIG_DIR`. With the sandbox the user's
/// `.credentials.json` is mounted over `<dir>/.credentials.json`, see the
/// module documentation. Without the sandbox the home needs its own login.
pub(crate) fn claude(dir: &Path, user: Option<&Path>, sandbox: bool) -> Result<Home, String> {
    create(dir)?;
    let home = Home { dir: dir.to_path_buf(), ..Home::default() };
    let target = dir.join(CLAUDE_CREDENTIALS);
    if own_login(&target) {
        return Ok(home);
    }
    let Some(shared) = user_file(user, CLAUDE_CREDENTIALS) else {
        // Nothing to share, for example with ANTHROPIC_API_KEY. claude says what is missing.
        return Ok(home);
    };
    if !sandbox {
        return Err(format!(
            "claude can share your login only inside the sandbox, because it replaces its credentials file when it saves a new \
             token. Switch the sandbox on (`[sandbox] enabled = true` in meerora.toml), or log in once for this agent with \
             `CLAUDE_CONFIG_DIR={} claude auth login`.",
            dir.display()
        ));
    }
    match std::fs::symlink_metadata(&target) {
        Ok(meta) if meta.is_file() => {}
        Ok(_) => return Err(format!("{} is not a regular file, so claude cannot use it. Remove it.", target.display())),
        // The mount point. bwrap would create it as well.
        Err(_) => {
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&target)
                .map_err(|e| format!("cannot create {}: {e}", target.display()))?;
        }
    }
    Ok(Home { files: vec![(shared, target)], ..home })
}

/// The session to resume in the claude home `dir`. A transcript that is
/// still in the user's own claude directory, from runs before the agent had
/// its own home, is copied over once. A session without a transcript
/// anywhere starts a new conversation, because claude refuses to resume it.
pub(crate) fn claude_session(dir: &Path, user: Option<&Path>, session: &str) -> Option<String> {
    let plain = !session.is_empty() && session.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if !plain {
        tracing::warn!("{session:?} is not a claude session id, so the run starts a new conversation");
        return None;
    }
    let file = format!("{session}.jsonl");
    if find_transcript(&dir.join("projects"), &file).is_some() {
        return Some(session.to_string());
    }
    if let Some((project, source)) = user.and_then(|u| find_transcript(&u.join("projects"), &file)) {
        let target = dir.join("projects").join(&project);
        let copied = std::fs::create_dir_all(&target).and_then(|()| std::fs::copy(&source, target.join(&file))).and_then(|_| {
            // Large tool results live in a folder named after the session.
            let extra = source.with_extension("");
            if extra.is_dir() { copy_dir(&extra, &target.join(session)) } else { Ok(()) }
        });
        match copied {
            Ok(()) => {
                tracing::info!("moved claude session {session} into {}", dir.display());
                return Some(session.to_string());
            }
            Err(e) => tracing::warn!("cannot copy claude session {session} into {} ({e})", dir.display()),
        }
    }
    tracing::warn!("claude session {session} has no transcript, so the run starts a new conversation");
    None
}

/// The project folder and path of `<projects>/*/<file>`.
fn find_transcript(projects: &Path, file: &str) -> Option<(String, PathBuf)> {
    std::fs::read_dir(projects).ok()?.filter_map(|e| e.ok()).find_map(|entry| {
        let path = entry.path().join(file);
        path.is_file().then(|| (entry.file_name().to_string_lossy().into_owned(), path))
    })
}

fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let (source, dest) = (entry.path(), to.join(entry.file_name()));
        let kind = entry.file_type()?;
        if kind.is_dir() {
            copy_dir(&source, &dest)?;
        } else if kind.is_file() {
            std::fs::copy(&source, &dest)?;
        }
    }
    Ok(())
}

/// Prepares `dir` as `CODEX_HOME`. It writes [`CODEX_CONFIG`] and links the
/// user's `auth.json`, see the module documentation.
pub(crate) fn codex(dir: &Path, user: Option<&Path>, sandbox: bool) -> Result<Home, String> {
    create(dir)?;
    let config = dir.join("config.toml");
    if std::fs::read_to_string(&config).ok().as_deref() != Some(CODEX_CONFIG) {
        let tmp = dir.join("config.toml.tmp");
        std::fs::write(&tmp, CODEX_CONFIG)
            .and_then(|()| std::fs::rename(&tmp, &config))
            .map_err(|e| format!("cannot write {}: {e}", config.display()))?;
    }
    let mut home = Home { dir: dir.to_path_buf(), files: Vec::new(), read_only: if sandbox { vec![config] } else { Vec::new() } };
    let link = dir.join(CODEX_AUTH);
    if own_login(&link) {
        return Ok(home);
    }
    let Some(shared) = user_file(user, CODEX_AUTH) else {
        if std::env::var_os("OPENAI_API_KEY").is_some() || std::env::var_os("CODEX_API_KEY").is_some() {
            return Ok(home);
        }
        return Err(format!(
            "codex is not logged in. Log in once with `codex login`, or for this agent only with `CODEX_HOME={} codex login`.",
            dir.display()
        ));
    };
    let current = std::fs::read_link(&link).ok();
    if current.as_deref() != Some(shared.as_path()) {
        // Only an old link or an empty file is replaced. A login of the home stays (see above).
        if std::fs::symlink_metadata(&link).is_ok() {
            std::fs::remove_file(&link).map_err(|e| format!("cannot replace {}: {e}", link.display()))?;
        }
        std::os::unix::fs::symlink(&shared, &link).map_err(|e| format!("cannot link {} to {}: {e}", link.display(), shared.display()))?;
    }
    if sandbox {
        home.files.push((shared.clone(), shared));
    }
    Ok(home)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::tests::Scratch;

    #[test]
    fn claude_shares_the_login_through_a_mount() {
        let s = Scratch::new("claude-home");
        let (home, user) = (s.0.join("runtimes/claude/wren"), s.0.join("user"));
        std::fs::create_dir_all(&user).unwrap();
        std::fs::write(user.join(CLAUDE_CREDENTIALS), "{}").unwrap();

        let prepared = claude(&home, Some(&user), true).unwrap();
        let target = home.join(CLAUDE_CREDENTIALS);
        assert_eq!(prepared.files, vec![(user.join(CLAUDE_CREDENTIALS).canonicalize().unwrap(), target.clone())]);
        let meta = std::fs::symlink_metadata(&target).unwrap();
        assert!(meta.is_file() && meta.len() == 0, "an empty mount point, never a copy");
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&home).unwrap().permissions().mode() & 0o777, 0o700);

        let err = claude(&home, Some(&user), false).unwrap_err();
        assert!(err.contains("claude auth login") && err.contains(&home.display().to_string()), "{err}");

        // A login of the home itself stays, with or without the sandbox.
        std::fs::write(&target, "{\"own\":true}").unwrap();
        assert!(claude(&home, Some(&user), true).unwrap().files.is_empty());
        assert!(claude(&home, Some(&user), false).is_ok());
        // With nothing to share, claude reports the missing login itself.
        assert!(claude(&s.0.join("other"), Some(&s.0.join("nobody")), false).unwrap().files.is_empty());
    }

    #[test]
    fn claude_sessions_move_into_the_home_once() {
        let s = Scratch::new("claude-session");
        let (home, user) = (s.0.join("home"), s.0.join("user"));
        let project = user.join("projects/-data-agents-wren");
        std::fs::create_dir_all(project.join("s-1/tool-results")).unwrap();
        std::fs::write(project.join("s-1.jsonl"), "{\"type\":\"user\"}\n").unwrap();
        std::fs::write(project.join("s-1/tool-results/a.txt"), "big").unwrap();

        assert_eq!(claude_session(&home, Some(&user), "s-1"), Some("s-1".into()));
        let moved = home.join("projects/-data-agents-wren");
        assert_eq!(std::fs::read_to_string(moved.join("s-1.jsonl")).unwrap(), "{\"type\":\"user\"}\n");
        assert_eq!(std::fs::read_to_string(moved.join("s-1/tool-results/a.txt")).unwrap(), "big");
        assert!(project.join("s-1.jsonl").is_file(), "the user's transcript stays where it is");
        // Found in the home from now on, even without the user's copy.
        assert_eq!(claude_session(&home, None, "s-1"), Some("s-1".into()));
        assert_eq!(claude_session(&home, Some(&user), "gone"), None);
        assert_eq!(claude_session(&home, Some(&user), "../etc/passwd"), None);
    }

    #[test]
    fn codex_links_the_login_and_writes_its_config() {
        let s = Scratch::new("codex-home");
        let (home, user) = (s.0.join("runtimes/codex/wren"), s.0.join("user"));
        std::fs::create_dir_all(&user).unwrap();
        std::fs::write(user.join(CODEX_AUTH), "{}").unwrap();
        std::fs::write(user.join("config.toml"), "model = \"mine\"\n").unwrap();
        let shared = user.join(CODEX_AUTH).canonicalize().unwrap();

        let prepared = codex(&home, Some(&user), true).unwrap();
        assert_eq!(std::fs::read_to_string(home.join("config.toml")).unwrap(), CODEX_CONFIG);
        assert_eq!(std::fs::read_link(home.join(CODEX_AUTH)).unwrap(), shared);
        assert_eq!(prepared.files, vec![(shared.clone(), shared.clone())]);
        assert_eq!(prepared.read_only, vec![home.join("config.toml")]);
        assert!(!home.join("rules").exists() && !home.join("AGENTS.md").exists());
        let parsed: toml::Table = toml::from_str(CODEX_CONFIG).unwrap();
        assert_eq!(parsed["cli_auth_credentials_store"].as_str(), Some("file"));

        // A changed config is written again, the link is kept.
        std::fs::write(home.join("config.toml"), "model = \"changed\"\n").unwrap();
        let prepared = codex(&home, Some(&user), false).unwrap();
        assert_eq!(std::fs::read_to_string(home.join("config.toml")).unwrap(), CODEX_CONFIG);
        assert!(prepared.files.is_empty() && prepared.read_only.is_empty(), "nothing to mount without the sandbox");
        assert_eq!(std::fs::read_link(home.join(CODEX_AUTH)).unwrap(), shared);

        // Writes through the link reach the user's file.
        std::fs::write(home.join(CODEX_AUTH), "{\"refreshed\":true}").unwrap();
        assert_eq!(std::fs::read_to_string(&shared).unwrap(), "{\"refreshed\":true}");
        assert!(std::fs::symlink_metadata(home.join(CODEX_AUTH)).unwrap().file_type().is_symlink());
    }

    #[test]
    fn codex_keeps_a_login_of_its_own() {
        let s = Scratch::new("codex-own");
        let (home, user) = (s.0.join("home"), s.0.join("user"));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&user).unwrap();
        std::fs::write(user.join(CODEX_AUTH), "{}").unwrap();
        std::fs::write(home.join(CODEX_AUTH), "{\"own\":true}").unwrap();
        assert!(codex(&home, Some(&user), true).unwrap().files.is_empty());
        assert_eq!(std::fs::read_to_string(home.join(CODEX_AUTH)).unwrap(), "{\"own\":true}");
    }
}
