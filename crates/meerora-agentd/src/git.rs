//! Agent directories as git repositories (D21). Uses the git command line
//! with the user's configuration switched off, so a personal config, signing
//! key or hook never interferes.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// What `.gitignore` of an agent directory starts with.
const GITIGNORE: &str = "artifacts/\nlogs/\n";

/// Whether the daemon keeps an agent directory in git.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Repo {
    /// The directory is its own repository.
    On,
    /// git is off for this directory, for the given reason.
    Off(String),
}

/// The empty tree. Attributes read from it are none, so no filter or
/// textconv driver from the agent's `.gitattributes` can run.
pub(crate) const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

/// git on an agent directory, which the agent can write. Besides the user's
/// configuration, nothing from the directory may run a command on the host:
/// no hooks, no fsmonitor and no attribute filters (the sandbox also keeps
/// `.git` read-only for the agent, this covers a switched-off sandbox).
fn git(dir: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_ATTR_SOURCE", EMPTY_TREE)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=meerora", "-c", "user.email=agents@meerora.local"])
        .args(["-c", "commit.gpgsign=false", "-c", "init.defaultBranch=main", "-c", "core.hooksPath=/dev/null"])
        .args(["-c", "core.fsmonitor=false", "-c", "protocol.ext.allow=never"]);
    // Drivers named through .git/info/attributes would still run (gitsafe).
    if let Err(e) = meerora_protocol::gitsafe::neutralize(&mut cmd, dir) {
        tracing::warn!("git is refused in {} because {e}", dir.display());
    }
    cmd
}

fn run(dir: &Path, args: &[&str]) -> Result<Output, String> {
    git(dir).args(args).output().map_err(|e| format!("cannot run git ({e})"))
}

fn checked(dir: &Path, args: &[&str]) -> Result<Output, String> {
    let out = run(dir, args)?;
    if out.status.success() {
        Ok(out)
    } else {
        Err(format!("git {} failed ({})", args.join(" "), String::from_utf8_lossy(&out.stderr).trim()))
    }
}

/// The top of the work tree that contains `dir`, if any.
fn toplevel(dir: &Path) -> Result<Option<PathBuf>, String> {
    let out = run(dir, &["rev-parse", "--show-toplevel"])?;
    Ok(out.status.success().then(|| PathBuf::from(String::from_utf8_lossy(&out.stdout).trim())))
}

/// Makes `dir` a repository with a `.gitignore`, unless it already is one.
/// A directory inside another work tree (such as `assets/` of the meerora
/// repository) never gets a nested repository.
pub fn ensure_repo(dir: &Path) -> Repo {
    let canonical = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    match toplevel(dir) {
        Err(e) => return Repo::Off(e),
        Ok(Some(top)) if top.canonicalize().unwrap_or(top.clone()) == canonical => {}
        Ok(Some(top)) => return Repo::Off(format!("it lies inside the work tree {}", top.display())),
        Ok(None) => {
            if let Err(e) = checked(dir, &["init", "-q"]) {
                return Repo::Off(e);
            }
        }
    }
    // Only into nothing, so a symlink the agent put there is not followed (safefs).
    if let Err(e) = meerora_protocol::safefs::create_new(dir, ".gitignore", GITIGNORE.as_bytes()) {
        return Repo::Off(format!("cannot write {} ({e})", dir.join(".gitignore").display()));
    }
    Repo::On
}

/// Commits everything that changed. Returns false when there was nothing to commit.
pub fn commit(dir: &Path, message: &str) -> Result<bool, String> {
    checked(dir, &["add", "-A"])?;
    let staged = run(dir, &["diff", "--cached", "--quiet"])?;
    if staged.status.success() {
        return Ok(false);
    }
    checked(dir, &["commit", "-q", "-m", message])?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::tests::TempDir;

    fn log(dir: &Path) -> String {
        String::from_utf8_lossy(&checked(dir, &["log", "--format=%an <%ae> %s"]).unwrap().stdout).into_owned()
    }

    /// A planted filter, fsmonitor or hook never runs when the daemon commits.
    #[test]
    fn nothing_in_the_agent_directory_runs_on_commit() {
        let tmp = TempDir::new("git-planted");
        let dir = tmp.0.join("wren");
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(ensure_repo(&dir), Repo::On);
        let pwned = tmp.0.join("PWNED");
        let p = pwned.display();
        std::fs::write(dir.join(".gitattributes"), "* filter=evil diff=evil\n").unwrap();
        let mut config = std::fs::read_to_string(dir.join(".git/config")).unwrap();
        config.push_str(&format!(
            "[filter \"evil\"]\n\tclean = sh -c 'touch {p}.clean; cat'\n\tsmudge = sh -c 'touch {p}.smudge; cat'\n\
             [diff \"evil\"]\n\ttextconv = sh -c 'touch {p}.textconv; cat'\n\
             [core]\n\tfsmonitor = sh -c 'touch {p}.fsmonitor'\n"
        ));
        // git reads .git/info/attributes even with GIT_ATTR_SOURCE set.
        config.push_str(&format!("[filter \"info\"]\n\tclean = sh -c 'touch {p}.info; cat'\n"));
        std::fs::create_dir_all(dir.join(".git/info")).unwrap();
        std::fs::write(dir.join(".git/info/attributes"), "*.md filter=info\n").unwrap();
        std::fs::write(dir.join(".git/config"), config).unwrap();
        std::fs::create_dir_all(dir.join(".git/hooks")).unwrap();
        std::fs::write(dir.join(".git/hooks/pre-commit"), format!("#!/bin/sh\ntouch {p}.hook\n")).unwrap();
        std::fs::write(dir.join("memory.md"), "remember\n").unwrap();

        assert!(commit(&dir, "run with a planted config").unwrap());
        run(&dir, &["diff", "HEAD~1", "--stat"]).unwrap();
        let ran: Vec<_> = ["clean", "smudge", "textconv", "fsmonitor", "hook", "info"]
            .iter()
            .filter(|k| tmp.0.join(format!("PWNED.{k}")).exists())
            .collect();
        assert!(ran.is_empty(), "these ran on the host: {ran:?}");
    }

    #[test]
    fn commits_in_its_own_repository() {
        let tmp = TempDir::new("git-own");
        let dir = tmp.0.join("wren");
        std::fs::create_dir_all(dir.join("logs")).unwrap();
        std::fs::create_dir_all(dir.join("memory")).unwrap();
        std::fs::write(dir.join("memory/notes.md"), "hello\n").unwrap();
        std::fs::write(dir.join("logs/messages.jsonl"), "{}\n").unwrap();

        assert_eq!(ensure_repo(&dir), Repo::On);
        assert_eq!(std::fs::read_to_string(dir.join(".gitignore")).unwrap(), GITIGNORE);
        assert_eq!(ensure_repo(&dir), Repo::On, "a second call keeps the repository");
        assert!(commit(&dir, "run 2026-10-14 10:00: Check mail").unwrap());
        assert!(!commit(&dir, "nothing").unwrap(), "nothing changed");
        assert_eq!(log(&dir).trim(), "meerora <agents@meerora.local> run 2026-10-14 10:00: Check mail");
        let files = checked(&dir, &["ls-files"]).unwrap();
        let files = String::from_utf8_lossy(&files.stdout);
        assert!(files.contains("memory/notes.md") && !files.contains("logs/"), "{files}");
    }

    #[test]
    fn skips_directories_inside_a_work_tree() {
        let tmp = TempDir::new("git-nested");
        checked(&tmp.0, &["init", "-q"]).unwrap();
        let dir = tmp.0.join("agents/wren");
        std::fs::create_dir_all(&dir).unwrap();
        match ensure_repo(&dir) {
            Repo::Off(reason) => assert!(reason.contains("inside the work tree"), "{reason}"),
            Repo::On => panic!("must not nest a repository"),
        }
        assert!(!dir.join(".git").exists());
        assert!(!dir.join(".gitignore").exists());
    }
}
