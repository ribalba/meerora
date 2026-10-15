//! The daemon finds the shipped assets through `$MEERORA_ASSETS`, the same
//! lookup that lets an installed binary find /usr/local/share/meerora
//! (scripts/install-session.sh). Both tests point it at a copy of the
//! repository's assets.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_meerora-agentd");

/// A directory below the system temp dir that is removed again at the end.
struct TestDir(PathBuf);

impl TestDir {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("meerora-agentd-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("home")).unwrap();
        Self(dir)
    }

    /// Copies the parts of the repository's assets that make a data dir, with a marker file.
    fn assets_copy(&self) -> PathBuf {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets");
        let copy = self.0.join("assets copy");
        for part in ["spaces", "space", "agents", "scenarios"] {
            if repo.join(part).is_dir() {
                copy_dir(&repo.join(part), &copy.join(part));
            }
        }
        fs::write(copy.join("spaces/from-the-copy.txt"), "marker\n").unwrap();
        copy
    }

    /// Runs the daemon's command line as a user without a data dir of their own.
    fn run(&self, assets: &Path, args: &[&str]) -> Output {
        Command::new(BIN)
            .args(args)
            .env_remove("MEERORA_DATA_DIR")
            .env_remove("XDG_DATA_HOME")
            .env("HOME", self.0.join("home"))
            .env("MEERORA_ASSETS", assets)
            .env("RUST_LOG", "error")
            .output()
            .unwrap()
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Copies a folder without runtime state such as `.git` and `logs`.
fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name();
        if [".git", "logs", "artifacts"].iter().any(|skip| name == *skip) {
            continue;
        }
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &to.join(&name));
        } else {
            fs::copy(entry.path(), to.join(&name)).unwrap();
        }
    }
}

#[test]
fn check_uses_meerora_assets_without_a_data_dir() {
    let tmp = TestDir::new("assets-check");
    let assets = tmp.assets_copy();
    let out = tmp.run(&assets, &["check"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains(&format!("data dir  {}", assets.display())), "{stdout}\n{}", String::from_utf8_lossy(&out.stderr));
}

#[test]
fn init_copies_from_meerora_assets() {
    let tmp = TestDir::new("assets-init");
    let assets = tmp.assets_copy();
    let target = tmp.0.join("data");
    let out = tmp.run(&assets, &["init", "--data-dir", target.to_str().unwrap()]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{stdout}\n{}", String::from_utf8_lossy(&out.stderr));
    assert!(stdout.contains(&format!("from {}", assets.display())), "{stdout}");
    assert_eq!(fs::read_to_string(target.join("spaces/from-the-copy.txt")).unwrap(), "marker\n");
    // A new data dir has Claude and the helper, the demo agents stay in the assets.
    let mut agents: Vec<String> =
        fs::read_dir(target.join("agents")).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
    agents.sort();
    assert_eq!(agents, ["claude", "rivet"]);
}
