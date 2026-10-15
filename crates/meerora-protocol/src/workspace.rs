//! Workspace folders of an agent: `workspace = ["~/code/project"]` in
//! `agent.toml`. They are folders the agent may read and write besides its
//! own directory, for example a code repository it works on.
//!
//! The user sets them in the builder or by hand. The agent cannot change
//! them, because `agent.toml` is read-only inside its sandbox. The sandbox
//! mounts each folder read-write and every `.git` that [`protected_git_dirs`]
//! lists read-only, so the agent can edit the code but not the git
//! configuration, hooks or index that the user's own git reads on the host.
//!
//! [`check`] refuses folders that would hand the agent far more than a
//! project: the whole computer, the home folder, system folders, folders with
//! keys and settings, and anything that holds meerora's data directory or lies
//! inside it (agents, CLI logins and the daemon's state live there).
//!
//! A `.git` is read-only only while it exists when a run starts. One that
//! appears while an agent can write the folder may hold a configuration the
//! agent wrote, with a filter that runs a command for whoever uses git there.
//! [`Trust`] (`<data dir>/workspace-trust.json`) records which `.git` entries
//! existed before any agent could write their folder and which appeared later.
//! The daemon records at the start and the end of every run, the Git app
//! asks [`Trust::verdict`] and runs your own git only on trusted ones.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Entries of a workspace folder that [`protected_git_dirs`] looks at for
/// nested repositories. A folder with more entries is only scanned this far,
/// so a huge folder costs little.
pub const MAX_SCAN: usize = 4000;

/// System folders. A workspace inside one of them is refused, unless it lies
/// inside the home folder (Fedora Atomic keeps homes in `/var/home`).
const SYSTEM: &[&str] =
    &["/usr", "/etc", "/bin", "/sbin", "/lib", "/lib32", "/lib64", "/boot", "/dev", "/proc", "/sys", "/run", "/var", "/opt", "/root"];

/// Folders that are refused themselves, while folders inside them are fine.
const NOT_ITSELF: &[&str] = &["/tmp", "/home", "/mnt", "/media", "/srv"];

/// Folders of the home with keys, logins and settings.
const PRIVATE: &[&str] = &[
    ".ssh",
    ".gnupg",
    ".aws",
    ".azure",
    ".kube",
    ".docker",
    ".config",
    ".local",
    ".cache",
    ".claude",
    ".codex",
    ".mozilla",
    ".password-store",
    ".pki",
];

/// `path` with a leading `~/` taken as `home`. None for a relative path or
/// for `~` without a home.
pub fn expand(path: &Path, home: Option<&Path>) -> Option<PathBuf> {
    if path == Path::new("~") {
        return home.map(Path::to_path_buf);
    }
    if let Ok(rest) = path.strip_prefix("~") {
        return home.map(|h| h.join(rest));
    }
    path.is_absolute().then(|| path.to_path_buf())
}

/// `path` with the home written as `~`, for showing and for `agent.toml`.
pub fn shorten(path: &Path, home: Option<&Path>) -> String {
    match home.and_then(|h| path.strip_prefix(h).ok()) {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// The folder as the sandbox mounts it (absolute, symlinks resolved), or a
/// sentence that says why it cannot be a workspace folder. `home` is the
/// user's home folder and `data_dir` meerora's data directory.
pub fn check(path: &Path, home: Option<&Path>, data_dir: &Path) -> Result<PathBuf, String> {
    let shown = path.display();
    let Some(full) = expand(path, home) else {
        return Err(format!("{shown} is not an absolute path. Write it like ~/code/project or /srv/project."));
    };
    let meta = std::fs::metadata(&full).map_err(|_| format!("{shown} does not exist."))?;
    if !meta.is_dir() {
        return Err(format!("{shown} is not a folder."));
    }
    let folder = canonical(&full);
    let home = home.map(canonical);
    let data = canonical(data_dir);
    if folder == Path::new("/") {
        return Err("/ is the whole computer. Pick a project folder.".into());
    }
    if let Some(home) = &home {
        if folder == *home {
            return Err(format!("{shown} is your whole home folder, with your keys and settings. Pick a project folder inside it."));
        }
        if home.starts_with(&folder) {
            return Err(format!("{shown} holds your home folder. Pick a project folder inside it."));
        }
        if let Ok(rest) = folder.strip_prefix(home)
            && let Some(first) = rest.components().next()
            && PRIVATE.iter().any(|p| first.as_os_str() == *p)
        {
            return Err(format!("{shown} holds your keys, logins or settings, which agents never get."));
        }
    }
    if data.starts_with(&folder) {
        return Err(format!("{shown} holds meerora's data folder with every agent, their logins and the daemon's state."));
    }
    if folder.starts_with(&data) {
        return Err(format!("{shown} lies inside meerora's data folder, where agents and their logins live."));
    }
    let in_home = home.as_ref().is_some_and(|h| folder.starts_with(h));
    if !in_home && SYSTEM.iter().any(|s| folder.starts_with(s)) {
        return Err(format!("{shown} is a system folder. Pick a project folder."));
    }
    if NOT_ITSELF.iter().any(|s| folder == Path::new(s)) {
        return Err(format!("{shown} is shared by many programs. Pick a project folder inside it."));
    }
    Ok(folder)
}

/// The workspace folders of an agent that pass [`check`], and a sentence for
/// each one that does not. Duplicates are dropped.
pub fn resolve(folders: &[PathBuf], home: Option<&Path>, data_dir: &Path) -> (Vec<PathBuf>, Vec<String>) {
    let (mut ok, mut refused) = (Vec::new(), Vec::new());
    for folder in folders {
        match check(folder, home, data_dir) {
            Ok(path) if !ok.contains(&path) => ok.push(path),
            Ok(_) => {}
            Err(why) => refused.push(why),
        }
    }
    (ok, refused)
}

/// Child folders of `folder` that are git repositories of their own, one
/// level deep. The sandbox binds each one onto itself, so the agent cannot
/// move it away and put a repository of its own in its place. Symlinks and
/// `.git` itself are skipped.
pub fn nested_repos(folder: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(folder) else { return Vec::new() };
    let mut out: Vec<PathBuf> = entries
        .take(MAX_SCAN)
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name() != ".git" && e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.path())
        .filter(|p| std::fs::symlink_metadata(p.join(".git")).is_ok())
        .collect();
    out.sort();
    out
}

/// The `.git` entries of a workspace folder that stay read-only for the
/// agent: the folder's own (listed even when it is missing, the sandbox skips
/// missing ones) and those of the repositories in [`nested_repos`].
pub fn protected_git_dirs(folder: &Path) -> Vec<PathBuf> {
    std::iter::once(folder.join(".git")).chain(nested_repos(folder).into_iter().map(|r| r.join(".git"))).collect()
}

/// The trust record in the data directory.
pub const TRUST_FILE: &str = "workspace-trust.json";

/// Which `.git` entries of workspace folders may get your own git, by their
/// canonical folder joined with `.git` (the entry itself is not resolved).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Trust {
    /// Present before any agent could write their folder, or trusted by you.
    #[serde(default)]
    pub trusted: BTreeSet<PathBuf>,
    /// Appeared while an agent could write there, with that agent's id.
    #[serde(default)]
    pub untrusted: BTreeMap<PathBuf, String>,
    /// Workspace folders that had a run, with the agent of the latest one.
    #[serde(default)]
    pub ran: BTreeMap<PathBuf, String>,
}

/// What [`Trust::verdict`] says about a `.git`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Your own git may run there.
    Trusted,
    /// It appeared while this agent could write there.
    Untrusted(String),
}

impl Trust {
    /// The record, empty when it is missing or unreadable.
    pub fn load(data_dir: &Path) -> Trust {
        std::fs::read_to_string(data_dir.join(TRUST_FILE)).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
    }

    /// The agent of a run in a folder that holds `git`, if one ran there.
    fn writer(&self, git: &Path) -> Option<&String> {
        self.ran.iter().filter(|(folder, _)| git.starts_with(folder)).map(|(_, agent)| agent).next()
    }

    /// Whether your own git may run with the `.git` at `git`. Unknown
    /// entries count as trusted until an agent ran in a folder that holds
    /// them, because only a run can have written them.
    pub fn verdict(&self, git: &Path) -> Verdict {
        if self.trusted.contains(git) {
            return Verdict::Trusted;
        }
        if let Some(agent) = self.untrusted.get(git).or_else(|| self.writer(git)) {
            return Verdict::Untrusted(agent.clone());
        }
        Verdict::Trusted
    }
}

/// The `.git` entries of `folder` that exist and that the sandbox keeps
/// read-only (see [`protected_git_dirs`]).
fn existing_git_dirs(folder: &Path) -> Vec<PathBuf> {
    protected_git_dirs(folder).into_iter().filter(|g| std::fs::symlink_metadata(g).is_ok()).collect()
}

/// Changes the record under a lock, so the daemon and the app never lose
/// each other's changes.
pub fn update_trust(data_dir: &Path, change: impl FnOnce(&mut Trust)) -> std::io::Result<()> {
    std::fs::create_dir_all(data_dir)?;
    let lock = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(data_dir.join("workspace-trust.lock"))?;
    lock.lock()?;
    let mut trust = Trust::load(data_dir);
    let before = trust.clone();
    change(&mut trust);
    if trust == before {
        return Ok(());
    }
    let path = data_dir.join(TRUST_FILE);
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(&trust).map_err(std::io::Error::other)?)?;
    std::fs::rename(&tmp, &path)
}

/// At the start of a run of `agent` with the checked workspace `folders`:
/// every `.git` the sandbox protects and the record does not know yet is
/// trusted when no run happened in a folder that holds it, else untrusted,
/// because it appeared while an agent could write there. Then the folders
/// count as having had a run.
pub fn record_run_start(data_dir: &Path, agent: &str, folders: &[PathBuf]) -> std::io::Result<()> {
    update_trust(data_dir, |trust| {
        for git in folders.iter().flat_map(|f| existing_git_dirs(f)) {
            if trust.trusted.contains(&git) || trust.untrusted.contains_key(&git) {
                continue;
            }
            match trust.writer(&git).cloned() {
                Some(writer) => {
                    trust.untrusted.insert(git, writer);
                }
                None => {
                    trust.trusted.insert(git);
                }
            }
        }
        for folder in folders {
            trust.ran.insert(folder.clone(), agent.to_string());
        }
    })
}

/// At the end of a run: a `.git` that exists now but that the record does
/// not know was not protected during the run, so it is untrusted.
pub fn record_run_end(data_dir: &Path, agent: &str, folders: &[PathBuf]) -> std::io::Result<()> {
    update_trust(data_dir, |trust| {
        for git in folders.iter().flat_map(|f| existing_git_dirs(f)) {
            if !trust.trusted.contains(&git) && !trust.untrusted.contains_key(&git) {
                trust.untrusted.insert(git, agent.to_string());
            }
        }
    })
}

/// You trust the `.git` at `git` after checking it.
pub fn trust_git_dir(data_dir: &Path, git: &Path) -> std::io::Result<()> {
    update_trust(data_dir, |trust| {
        trust.untrusted.remove(git);
        trust.trusted.insert(git.to_path_buf());
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Tmp(PathBuf);

    impl Tmp {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("meerora-ws-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir.canonicalize().unwrap())
        }
    }

    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn paths_expand_and_shorten() {
        let home = Path::new("/home/u");
        assert_eq!(expand(Path::new("~/code/p"), Some(home)), Some(PathBuf::from("/home/u/code/p")));
        assert_eq!(expand(Path::new("~"), Some(home)), Some(PathBuf::from("/home/u")));
        assert_eq!(expand(Path::new("/srv/p"), None), Some(PathBuf::from("/srv/p")));
        assert_eq!(expand(Path::new("code/p"), Some(home)), None);
        assert_eq!(expand(Path::new("~/p"), None), None);
        assert_eq!(shorten(Path::new("/home/u/code/p"), Some(home)), "~/code/p");
        assert_eq!(shorten(Path::new("/srv/p"), Some(home)), "/srv/p");
    }

    #[test]
    fn project_folders_pass_and_the_rest_is_refused() {
        let t = Tmp::new("check");
        let home = t.0.join("home");
        let data = home.join("meerora-data");
        for dir in [home.join("code/project"), data.join("agents/wren"), home.join(".ssh"), home.join(".local/share")] {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::fs::write(home.join("file.txt"), "x").unwrap();
        std::os::unix::fs::symlink(home.join("code/project"), home.join("link")).unwrap();
        let check = |p: &str| check(Path::new(p), Some(&home), &data);

        assert_eq!(check("~/code/project"), Ok(home.join("code/project")));
        assert_eq!(check(&home.join("code/project").display().to_string()), Ok(home.join("code/project")));
        assert_eq!(check("~/link"), Ok(home.join("code/project")), "symlinks are resolved");
        assert!(check("~/code").is_ok(), "a folder with many projects is fine");

        let refused = |p: &str, needle: &str| {
            let why = check(p).unwrap_err();
            assert!(why.contains(needle), "{p}: {why}");
        };
        refused("/", "whole computer");
        refused("~", "whole home folder");
        refused(&t.0.display().to_string(), "holds your home folder");
        refused("~/.ssh", "keys");
        refused("~/.local/share", "keys");
        refused("~/meerora-data", "meerora's data folder");
        refused("~/meerora-data/agents/wren", "inside meerora's data folder");
        refused("/usr/share", "system folder");
        refused("/etc", "system folder");
        if let Some(shared) = NOT_ITSELF.iter().find(|p| Path::new(p).is_dir() && !home.starts_with(p)) {
            refused(shared, "shared by many programs");
        }
        refused("~/missing", "does not exist");
        refused("~/file.txt", "not a folder");
        refused("code/project", "not an absolute path");

        // A data directory outside the home: its parents are refused too.
        let other = t.0.join("data");
        std::fs::create_dir_all(other.join("agents")).unwrap();
        let why = super::check(&t.0, None, &other).unwrap_err();
        assert!(why.contains("holds meerora's data folder"), "{why}");

        let (ok, refused) = resolve(
            &[PathBuf::from("~/code/project"), PathBuf::from("~/link"), PathBuf::from("/"), PathBuf::from("~/missing")],
            Some(&home),
            &data,
        );
        assert_eq!(ok, vec![home.join("code/project")], "duplicates are dropped");
        assert_eq!(refused.len(), 2);
    }

    #[test]
    fn git_dirs_of_the_folder_and_one_level_down() {
        let t = Tmp::new("git");
        let ws = t.0.join("ws");
        for dir in [ws.join(".git"), ws.join("lib/.git"), ws.join("docs"), ws.join("deep/inner/.git")] {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::fs::create_dir_all(ws.join("sub")).unwrap();
        std::fs::write(ws.join("sub/.git"), "gitdir: ../.git/modules/sub\n").unwrap();
        std::os::unix::fs::symlink(ws.join("lib"), ws.join("lib-link")).unwrap();
        assert_eq!(nested_repos(&ws), vec![ws.join("lib"), ws.join("sub")], "symlinks and deeper repositories are skipped");
        assert_eq!(protected_git_dirs(&ws), vec![ws.join(".git"), ws.join("lib/.git"), ws.join("sub/.git")]);
        let plain = t.0.join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        assert_eq!(protected_git_dirs(&plain), vec![plain.join(".git")], "the own .git is listed even when missing");
    }

    #[test]
    fn a_git_dir_that_appears_while_an_agent_can_write_is_untrusted() {
        let t = Tmp::new("trust");
        let (data, ws) = (t.0.join("data"), t.0.join("code/project"));
        std::fs::create_dir_all(ws.join(".git")).unwrap();
        std::fs::create_dir_all(ws.join("lib")).unwrap();
        let folders = vec![ws.clone()];
        assert_eq!(Trust::load(&data).verdict(&ws.join(".git")), Verdict::Trusted, "before any run nothing could write it");

        record_run_start(&data, "wren", &folders).unwrap();
        let trust = Trust::load(&data);
        assert!(trust.trusted.contains(&ws.join(".git")), "present before the first run");
        assert_eq!(trust.ran.get(&ws).map(String::as_str), Some("wren"));

        // During the run the agent makes a repository one level down.
        std::fs::create_dir_all(ws.join("lib/.git")).unwrap();
        record_run_end(&data, "wren", &folders).unwrap();
        let trust = Trust::load(&data);
        assert_eq!(trust.verdict(&ws.join("lib/.git")), Verdict::Untrusted("wren".into()));
        assert_eq!(trust.verdict(&ws.join(".git")), Verdict::Trusted);

        // Between runs, and unknown to the record: untrusted at the next start too.
        std::fs::create_dir_all(ws.join("docs/.git")).unwrap();
        assert_eq!(Trust::load(&data).verdict(&ws.join("docs/.git")), Verdict::Untrusted("wren".into()), "a folder that had a run");
        record_run_start(&data, "ember", &folders).unwrap();
        assert_eq!(Trust::load(&data).untrusted.get(&ws.join("docs/.git")).map(String::as_str), Some("wren"));

        // A folder inside another agent's workspace counts as written too.
        let inner = ws.join("docs");
        record_run_start(&data, "moth", std::slice::from_ref(&inner)).unwrap();
        assert!(!Trust::load(&data).trusted.contains(&inner.join(".git")));

        trust_git_dir(&data, &ws.join("lib/.git")).unwrap();
        let trust = Trust::load(&data);
        assert_eq!(trust.verdict(&ws.join("lib/.git")), Verdict::Trusted, "you trusted it");
        assert!(!trust.untrusted.contains_key(&ws.join("lib/.git")));
        let elsewhere = t.0.join("elsewhere/.git");
        assert_eq!(trust.verdict(&elsewhere), Verdict::Trusted, "repositories outside every workspace stay as they are");
    }
}
