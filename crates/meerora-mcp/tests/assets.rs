//! Without a data dir of the user's own, the bridge serves from
//! `$MEERORA_ASSETS`, the same lookup that lets an installed binary find
//! /usr/local/share/meerora (scripts/install-session.sh).

use std::path::PathBuf;
use std::process::{Command, Stdio};

const BIN: &str = env!("CARGO_BIN_EXE_meerora-mcp");

/// A directory below the system temp dir that is removed again at the end.
struct TestDir(PathBuf);

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn serves_from_meerora_assets_without_a_data_dir() {
    let tmp = TestDir(std::env::temp_dir().join(format!("meerora-mcp-assets-{}", std::process::id())));
    let assets = tmp.0.join("assets copy");
    std::fs::create_dir_all(assets.join("spaces")).unwrap();
    std::fs::create_dir_all(assets.join("agents/wren")).unwrap();
    std::fs::write(assets.join("agents/wren/agent.toml"), "name = \"Wren\"\nhome = \"desk\"\n").unwrap();
    std::fs::create_dir_all(tmp.0.join("home")).unwrap();

    let out = Command::new(BIN)
        .args(["--agent", "wren", "--socket"])
        .arg(tmp.0.join("none.sock"))
        .env_remove("MEERORA_DATA_DIR")
        .env_remove("XDG_DATA_HOME")
        .env("HOME", tmp.0.join("home"))
        .env("MEERORA_ASSETS", &assets)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert!(stderr.contains(&format!("serving agent wren from {} with", assets.display())), "{stderr}");
}
