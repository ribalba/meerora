//! Where the binaries find the shipped space, agents and scenarios. They seed
//! a new data dir (`meerora-agentd init`) and serve as the data dir while the
//! user has none of their own (see [`crate::resolve_data_dir`]).

use std::path::{Path, PathBuf};

/// The prefix `scripts/install-session.sh` installs to by default.
pub const INSTALLED_ASSETS: &str = "/usr/local/share/meerora";

/// The assets folder in this order:
///
/// 1. `$MEERORA_ASSETS` when it is set and not empty.
/// 2. `<prefix>/share/meerora` of the running binary, so an installed
///    `/usr/local/bin/meerora` finds `/usr/local/share/meerora` and a build
///    in `target/` never picks up an installed copy.
/// 3. `repo_assets`, the `assets/` folder of the repository during development.
/// 4. [`INSTALLED_ASSETS`], for a binary that was copied somewhere else.
///
/// A candidate counts when it has a `spaces/` folder with the rooms, or a
/// `space/` folder like assets from before rooms.
pub fn assets_dir(repo_assets: &Path) -> PathBuf {
    let exe = std::env::current_exe().ok();
    find_assets(std::env::var_os("MEERORA_ASSETS").map(PathBuf::from), exe.as_deref(), repo_assets)
}

/// [`assets_dir`] with the environment and the binary's path passed in.
pub fn find_assets(env: Option<PathBuf>, exe: Option<&Path>, repo_assets: &Path) -> PathBuf {
    if let Some(dir) = env.filter(|d| !d.as_os_str().is_empty()) {
        return dir;
    }
    let installed = exe.and_then(Path::parent).and_then(Path::parent).map(|prefix| prefix.join("share").join("meerora"));
    if let Some(dir) = installed.filter(|d| is_assets(d)) {
        return dir;
    }
    if is_assets(repo_assets) {
        return repo_assets.to_path_buf();
    }
    PathBuf::from(INSTALLED_ASSETS)
}

fn is_assets(dir: &Path) -> bool {
    dir.join("spaces").is_dir() || dir.join("space").is_dir()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory below the system temp dir that is removed again at the end.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("meerora-protocol-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn assets_in(dir: &Path) -> PathBuf {
        std::fs::create_dir_all(dir.join("space")).unwrap();
        dir.to_path_buf()
    }

    #[test]
    fn the_environment_wins() {
        let tmp = TempDir::new("assets-env");
        let repo = assets_in(&tmp.0.join("repo/assets"));
        let prefix = tmp.0.join("usr");
        assets_in(&prefix.join("share/meerora"));
        let exe = prefix.join("bin/meerora");
        let copy = tmp.0.join("copy");
        assert_eq!(find_assets(Some(copy.clone()), Some(&exe), &repo), copy);
        // An empty value counts as unset.
        assert_eq!(find_assets(Some(PathBuf::new()), Some(&exe), &repo), prefix.join("share/meerora"));
    }

    #[test]
    fn an_installed_binary_finds_its_prefix() {
        let tmp = TempDir::new("assets-prefix");
        let repo = assets_in(&tmp.0.join("repo/assets"));
        let prefix = tmp.0.join("usr/local");
        let share = assets_in(&prefix.join("share/meerora"));
        assert_eq!(find_assets(None, Some(&prefix.join("bin/meerora")), &repo), share);
    }

    #[test]
    fn a_build_in_the_repository_uses_the_repository() {
        let tmp = TempDir::new("assets-repo");
        let repo = assets_in(&tmp.0.join("repo/assets"));
        let exe = tmp.0.join("repo/target/debug/meerora");
        assert_eq!(find_assets(None, Some(&exe), &repo), repo);
        assert_eq!(find_assets(None, None, &repo), repo);
    }

    #[test]
    fn without_any_assets_the_default_prefix_remains() {
        let tmp = TempDir::new("assets-none");
        let exe = tmp.0.join("bin/meerora");
        assert_eq!(find_assets(None, Some(&exe), &tmp.0.join("gone")), PathBuf::from(INSTALLED_ASSETS));
    }
}
