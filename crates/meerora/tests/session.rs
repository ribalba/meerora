//! The login session (D14, D15). Runs scripts/tests/session-test.sh, which
//! tests meerora-session with a fake cage and the installer and uninstaller
//! in a staging root, and checks that the app finds `$MEERORA_ASSETS`.

use std::path::{Path, PathBuf};
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_meerora");

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

#[test]
fn session_scripts() {
    let out = Command::new("bash").arg(repo().join("scripts/tests/session-test.sh")).output().expect("bash runs");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{stdout}\n{}", String::from_utf8_lossy(&out.stderr));
}

/// A directory below the system temp dir that is removed again at the end.
struct TestDir(PathBuf);

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The app logs its data dir before it reads its own flags, so an unknown
/// flag shows the lookup without opening a window.
#[test]
fn the_app_uses_meerora_assets_without_a_data_dir() {
    let tmp = TestDir(std::env::temp_dir().join(format!("meerora-app-assets-{}", std::process::id())));
    let assets = tmp.0.join("assets copy");
    std::fs::create_dir_all(assets.join("spaces")).unwrap();
    std::fs::create_dir_all(tmp.0.join("home")).unwrap();
    let assets = assets.canonicalize().unwrap();

    let out = Command::new(BIN)
        .arg("--no-such-flag")
        .env_remove("MEERORA_DATA_DIR")
        .env_remove("XDG_DATA_HOME")
        .env("HOME", tmp.0.join("home"))
        .env("MEERORA_ASSETS", &assets)
        .env("RUST_LOG", "info")
        .env("NO_COLOR", "1")
        .output()
        .unwrap();
    let all = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(!out.status.success(), "{all}");
    assert!(all.contains(&format!("data dir {}", assets.display())), "{all}");
    assert!(all.contains("unknown argument"), "{all}");
}
