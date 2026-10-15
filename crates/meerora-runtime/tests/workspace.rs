//! A fake claude CLI in the real sandbox with a workspace folder: it gets the
//! folder with `--add-dir`, can write code there, and cannot change its
//! `.git`. This binary holds one test, because it gives the process a home of
//! its own (no user configuration or login is read).

mod common;

use std::path::Path;
use std::time::Duration;

use common::*;
use meerora_protocol::{PathSettings, RuntimeConfig, RuntimeKind, Settings};
use meerora_runtime::{Isolation, Permissions, create};

#[test]
fn a_sandboxed_run_works_in_its_workspace_but_not_in_its_git() {
    let root = scratch("ws-run");
    let home = root.join("home");
    let (agent, ws, runtimes) = (home.join("data/agents/wren"), home.join("code/project"), root.join("runtimes"));
    for dir in [agent.clone(), ws.join(".git"), ws.join("src"), runtimes.clone()] {
        std::fs::create_dir_all(dir).unwrap();
    }
    std::fs::write(ws.join(".git/config"), "[core]\n").unwrap();
    // SAFETY: the only test of this binary, set before any thread starts.
    unsafe {
        std::env::set_var("HOME", &home);
        std::env::set_var("CLAUDE_CONFIG_DIR", home.join(".claude"));
    }
    if meerora_runtime::sandbox::check().is_err() {
        eprintln!("note: the sandbox does not work here, so the workspace run is skipped");
        return;
    }
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let stream = bin.join("stream.jsonl");
    std::fs::write(
        &stream,
        "{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s1\"}\n{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"done\",\"session_id\":\"s1\"}\n",
    )
    .unwrap();
    let w = ws.display();
    let body = format!(
        "#!/bin/sh\nfor a in \"$@\"; do printf '%s\\n' \"$a\"; done > {w}/args\necho 'fn main() {{}}' > {w}/src/main.rs\n\
         if echo evil >> {w}/.git/config 2>/dev/null; then echo wrote > {w}/git-written; fi\ncat {}\n",
        stream.display()
    );
    let fake = script(&bin.join("fake-claude"), &body);
    let settings = Settings { paths: PathSettings { claude: Some(fake), ..Default::default() }, ..Default::default() };
    let runtime = create(&RuntimeConfig { kind: RuntimeKind::ClaudeCli, model: None, url: None }, &settings).unwrap();
    let mut req = request(&agent, "edit the project", Permissions { write: true, shell: false, web: false }, None);
    req.isolation = Some(Isolation {
        runtimes,
        sandbox: true,
        read: vec![bin.clone()],
        sockets: Vec::new(),
        workspace: vec![ws.canonicalize().unwrap()],
    });
    let events = collect(&runtime.start(req).unwrap(), Duration::from_secs(20));
    assert_eq!(result(&events), Ok("done".into()));
    let args = std::fs::read_to_string(ws.join("args")).expect("the fake ran and wrote into the workspace");
    assert!(args.lines().any(|a| a == format!("--add-dir={}", ws.canonicalize().unwrap().display())), "{args}");
    assert_eq!(std::fs::read_to_string(ws.join("src/main.rs")).unwrap(), "fn main() {}\n");
    assert!(!ws.join("git-written").exists(), "the .git of the workspace stays read-only");
    assert_eq!(std::fs::read_to_string(ws.join(".git/config")).unwrap(), "[core]\n");
    assert!(!Path::new(&agent).join("args").exists(), "the run still starts in the agent directory");
}
