//! Tests of the Git app with temporary repositories. git gets a home of its
//! own (repo::tests::sandbox_env), so no configuration of yours is used.

use super::repo::tests::{finish, open_user, sh, user_repo};
use super::*;
use crate::ui::editor::review::tests::TempDir;

/// Runs the instance's background work until it is idle.
fn settle(inst: &mut Instance) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        inst.update(0.0);
        let busy = inst.opening.is_some()
            || inst.poll.is_some()
            || inst.load.is_some()
            || inst.action.is_some()
            || inst.refresh
            || (inst.git.is_some() && inst.status.is_none())
            || (inst.selected.is_some() && inst.file.is_none());
        if !busy {
            return;
        }
        assert!(std::time::Instant::now() < deadline, "the Git app did not settle");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn open_sync(git: Git) -> Instance {
    let mut inst = Instance::default();
    let (hooks, remotes) = (git.hooks_in_tree(), git.remotes());
    inst.opened(Opened { git, hooks, remotes, warning: None });
    settle(&mut inst);
    inst
}

fn doc(inst: &Instance) -> &Doc {
    inst.file.as_ref().and_then(FileView::doc).expect("a text diff")
}

/// Ember's change: a longer sleep at the top and a new function at the end, far enough apart for two hunks.
fn changed_run_py(repo: &Path) {
    let text = std::fs::read_to_string(repo.join("run.py")).unwrap().replace("sleep(1)", "sleep(2)");
    std::fs::write(repo.join("run.py"), format!("{text}\n\ndef extra():\n    return 3\n")).unwrap();
}

#[test]
fn hunks_stage_unstage_and_discard() {
    let tmp = TempDir::new("git-hunks");
    let (repo, env) = user_repo(&tmp);
    changed_run_py(&repo);
    let mut inst = open_sync(open_user(&repo, &env));
    assert_eq!(inst.selected, Some((Section::Changes, "run.py".to_string())), "the changed file is picked");
    assert_eq!(doc(&inst).changes(), 2, "the sleep and the new function are two hunks");

    // Stage the first hunk only.
    inst.hunk(0, HunkAction::Stage);
    settle(&mut inst);
    let staged = sh(&env, &repo, &["diff", "--cached"]);
    assert!(staged.contains("+    time.sleep(2)") && !staged.contains("extra"), "{staged}");
    let unstaged = sh(&env, &repo, &["diff"]);
    assert!(unstaged.contains("+def extra():") && !unstaged.contains("sleep(2)"), "{unstaged}");
    assert_eq!(doc(&inst).changes(), 1, "the diff against the index shrank");

    // Unstage it again from the staged side.
    inst.select(Section::Staged, "run.py".into());
    settle(&mut inst);
    assert_eq!(doc(&inst).changes(), 1);
    inst.hunk(0, HunkAction::Unstage);
    settle(&mut inst);
    assert_eq!(sh(&env, &repo, &["diff", "--cached"]), "", "nothing is staged any more");
    assert_eq!(inst.selected, Some((Section::Changes, "run.py".to_string())), "the selection follows the file");

    // Discard the new function: the work tree goes back to the index there only.
    let h = doc(&inst)
        .hunks
        .iter()
        .position(|h| doc(&inst).lines[h.lines.clone()].iter().any(|l| l.new.is_some_and(|n| doc(&inst).work[n].contains("extra"))))
        .unwrap();
    inst.hunk(h, HunkAction::Discard);
    settle(&mut inst);
    let text = std::fs::read_to_string(repo.join("run.py")).unwrap();
    assert!(text.contains("time.sleep(2)") && !text.contains("extra"), "{text}");
    assert!(!repo.join(".git/index.lock").exists());
}

#[test]
fn editing_in_the_diff_writes_the_file() {
    let tmp = TempDir::new("git-edit");
    let (repo, env) = user_repo(&tmp);
    changed_run_py(&repo);
    let mut inst = open_sync(open_user(&repo, &env));
    let line = doc(&inst).work.iter().position(|l| l.contains("sleep(2)")).unwrap();
    inst.start_edit(line, 0);
    let edit = inst.edit.as_mut().unwrap();
    edit.text = "    time.sleep(3)".into();
    inst.edit_applied();
    assert!(inst.dirty());
    // Unsaved edits block staging, committing and switching files.
    inst.stage_file("run.py".into());
    assert_eq!(inst.confirm, Some(Confirm::Unsaved));
    assert!(inst.action.is_none(), "nothing ran");
    inst.select(Section::Untracked, "other.txt".into());
    assert_eq!(inst.selected, Some((Section::Changes, "run.py".to_string())), "the file stays");
    inst.subject = "Sleep longer".into();
    assert_eq!(inst.commit_blocker(), Some("Save or drop your edits first."));

    inst.save();
    assert!(!inst.dirty());
    let text = std::fs::read_to_string(repo.join("run.py")).unwrap();
    assert!(text.contains("    time.sleep(3)\n") && text.contains("def extra():"), "{text}");
    settle(&mut inst);
    assert!(doc(&inst).work_text().contains("sleep(3)"), "the diff shows the saved file");
    assert!(sh(&env, &repo, &["diff"]).contains("+    time.sleep(3)"));

    // Enter splits a line, Backspace at its start joins it again.
    let line = doc(&inst).work.iter().position(|l| l.contains("return 3")).unwrap();
    inst.start_edit(line, 4);
    if let Some(FileView { content: Content::Text(d), .. }) = &mut inst.file {
        let next = d.split_line(line, 4);
        assert_eq!(d.work[next], "return 3\n");
        d.join_previous(next);
    }
    inst.save();
    assert!(std::fs::read_to_string(repo.join("run.py")).unwrap().contains("    return 3\n"));
}

#[test]
fn commit_from_the_box_with_amend() {
    let tmp = TempDir::new("git-box");
    let (repo, env) = user_repo(&tmp);
    std::fs::write(repo.join("notes.md"), "# Notes\n\none\nthree\n").unwrap();
    let mut inst = open_sync(open_user(&repo, &env));
    assert_eq!(inst.commit_blocker(), Some("Stage a change to commit it."));
    inst.stage_all();
    settle(&mut inst);
    assert_eq!(inst.commit_blocker(), Some("Write a subject."), "an empty message is refused");
    inst.commit(false);
    assert!(inst.action.is_none());
    inst.subject = "Three instead of two".into();
    inst.body = "The notes said two.".into();
    inst.commit(false);
    settle(&mut inst);
    assert!(inst.note.as_ref().is_some_and(|(n, e)| n.starts_with("Committed ") && !e), "{:?}", inst.note);
    assert!(inst.subject.is_empty() && inst.body.is_empty());
    assert_eq!(sh(&env, &repo, &["log", "-1", "--format=%s|%b"]), "Three instead of two|The notes said two.");

    inst.amend = true;
    inst.subject = "Three, not two".into();
    assert_eq!(inst.commit_blocker(), None, "amend needs nothing staged");
    inst.commit(false);
    settle(&mut inst);
    assert_eq!(sh(&env, &repo, &["rev-list", "--count", "HEAD"]), "2");
    assert_eq!(sh(&env, &repo, &["log", "-1", "--format=%s"]), "Three, not two");
}

#[test]
fn push_without_upstream_offers_to_set_it() {
    let tmp = TempDir::new("git-box-push");
    let (repo, env) = user_repo(&tmp);
    let remote = tmp.0.join("remote.git");
    sh(&env, &tmp.0, &["init", "-q", "--bare", remote.to_str().unwrap()]);
    sh(&env, &repo, &["remote", "add", "origin", remote.to_str().unwrap()]);
    let mut inst = open_sync(open_user(&repo, &env));
    inst.push();
    assert_eq!(inst.confirm, Some(Confirm::Upstream { remote: "origin".into(), branch: "main".into() }));
    inst.confirm = None;
    inst.start_remote(Some(("origin".into(), "main".into())));
    let remote_job = inst.remote.take().unwrap();
    finish(remote_job).unwrap();
    inst.refresh = true;
    settle(&mut inst);
    let status = inst.status_ok().unwrap();
    assert_eq!((status.upstream.as_deref(), status.ahead), (Some("origin/main"), 0));
}

#[test]
fn the_agent_directory_uses_the_hardened_git_and_folds_your_edit_into_the_review() {
    let repo_dir = crate::ui::editor::review::tests::agent_repo("git-agent-dir");
    let dir = repo_dir.0.canonicalize().unwrap();
    let areas = Areas { agents: vec![dir.clone()], workspaces: Vec::new() };
    let home = TempDir::new("git-agent-home");
    let git = repo::open(&dir, &areas, &Default::default(), &super::repo::tests::sandbox_env(&home.0)).unwrap();
    assert_eq!(git.mode, Mode::Agent);
    let review = crate::ui::editor::review::Repo::open(&dir).unwrap();
    review.reviewed().unwrap();
    std::fs::write(dir.join("run.py"), "import time\n\ndef run():\n    return 2\n").unwrap();
    let mut inst = open_sync(git);
    let line = doc(&inst).work.iter().position(|l| l.contains("import time")).unwrap();
    inst.start_edit(line, 0);
    inst.edit.as_mut().unwrap().text = "import os".into();
    inst.edit_applied();
    inst.save();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while review.reviewed_text(Path::new("run.py")).unwrap().as_deref() != Some("import os\n\ndef run():\n    return 1\n") {
        assert!(std::time::Instant::now() < deadline, "your edit did not join the review");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn the_agent_is_named_when_it_changed_the_repository() {
    let tmp = TempDir::new("git-d12");
    let (repo, env) = user_repo(&tmp);
    let git = open_user(&repo, &env);
    let entry = |at: f64, tool: &str, summary: String| ActionEntry { at, tool: tool.into(), summary, target: None };
    let actions = vec![
        entry(10.0, "editor.patch", repo.join("run.py").display().to_string()),
        entry(20.0, "terminal.run", "cargo test".into()),
        entry(30.0, "editor.patch", format!("memory/notes.md, {}", repo.join("src/lib.rs").display())),
        entry(40.0, "editor.patch", "/elsewhere/x.rs".into()),
    ];
    let (ago_s, touched) = agent_changes(&actions, &git, Some(&tmp.0.join("agent")), 150.0);
    assert_eq!(ago_s, Some(120.0), "the newest patch inside the repository");
    assert_eq!(touched, HashSet::from(["run.py".to_string(), "src/lib.rs".to_string()]));
    assert_eq!(ago(120.0), "2 min ago");
    assert_eq!(ago(2.0), "just now");
    assert_eq!(agent_changes(&actions[1..2], &git, None, 0.0).0, None);
    // A long path comes cut in the summary and whole in the target.
    let target = Some(repo.join("src/lib.rs").display().to_string());
    let long = ActionEntry { at: 50.0, tool: "editor.patch".into(), summary: "/tmp/…".into(), target };
    assert_eq!(agent_changes(&[long], &git, None, 60.0).1, HashSet::from(["src/lib.rs".to_string()]));
}

#[test]
fn the_tile_draws_without_a_window() {
    let tmp = TempDir::new("git-draw");
    let (repo, env) = user_repo(&tmp);
    changed_run_py(&repo);
    std::fs::write(repo.join("notes.md"), "# Notes\n\none\nthree\n").unwrap();
    std::fs::write(repo.join("new.txt"), "fresh\n").unwrap();
    sh(&env, &repo, &["add", "notes.md"]);
    let mut inst = open_sync(open_user(&repo, &env));
    let ctx = egui::Context::default();
    crate::theme::install(&ctx);
    let screen = Rect::from_min_size(egui::Pos2::ZERO, vec2(1300.0, 820.0));
    let recent: Vec<PathBuf> = vec![repo.clone()];
    for (width, unified, edit, picker) in
        [(1200.0, false, false, false), (1200.0, true, true, false), (560.0, false, false, false), (900.0, false, false, true)]
    {
        inst.unified = unified;
        inst.picker = picker;
        if edit {
            inst.start_edit(4, 2);
        }
        let view = View {
            agent: "ember".into(),
            n: 1,
            name: "Ember".into(),
            color: [0xF2, 0xB4, 0x5A],
            selected: true,
            agent_dir: Some(tmp.0.join("agent")),
            workspace: vec![("~/code/project".into(), Ok(repo.clone())), ("/".into(), Err("/ is the whole computer.".into()))],
            recent: &recent,
            changed_ago: Some(75.0),
            touched: HashSet::from(["run.py".to_string()]),
            made_by: None,
        };
        for _ in 0..2 {
            let raw = egui::RawInput { screen_rect: Some(screen), ..Default::default() };
            let mut output = ctx.run_ui(raw, |ui| {
                let head = Rect::from_min_size(pos2(10.0, 10.0), vec2(width, 38.0));
                let body = Rect::from_min_max(pos2(11.0, 49.0), pos2(10.0 + width, 810.0));
                let mut out = Out::default();
                inst.draw(ui, &view, head, body, 0.0, &mut out);
                assert!(out.open.is_none());
            });
            output.textures_delta.clear();
        }
    }
    // What an agent reads of the instance (D30).
    let read = inst.reading();
    assert_eq!(read["summary"], "project on main: 1 staged, 1 changed, 1 untracked, no upstream");
    let content = read["content"].as_str().unwrap();
    assert!(content.contains("staged:\n  M notes.md") && content.contains("untracked:\n  U new.txt"), "{content}");
    assert_eq!(read["elements"].as_array().map(Vec::len), Some(3));
}

#[test]
fn recent_repositories_are_kept() {
    let tmp = TempDir::new("git-recent");
    let mut store = Store::load(&tmp.0);
    for i in 0..15 {
        store.remember("ember", 1, &tmp.0.join(format!("r{i}")));
    }
    store.remember("ember", 2, &tmp.0.join("r3"));
    let again = Store::load(&tmp.0);
    assert_eq!(again.saved.recent.len(), MAX_RECENT);
    assert_eq!(again.saved.recent[0], tmp.0.join("r3"), "the newest first, without duplicates");
    assert_eq!(again.saved.open["ember"][&1], tmp.0.join("r14"));
    assert_eq!(again.saved.open["ember"][&2], tmp.0.join("r3"));
}

/// A workspace repository whose `.git` appeared after an agent ran there:
/// planted with a filter in its configuration and attributes, and a change
/// of the same size, so a normal status would hash it through the filter.
fn planted_repo(tmp: &TempDir) -> (PathBuf, Vec<(std::ffi::OsString, std::ffi::OsString)>, PathBuf, Areas) {
    let (repo, env) = user_repo(tmp);
    let marker = tmp.0.join("filter-ran");
    sh(&env, &repo, &["config", "filter.evil=x.clean", &format!("sh -c 'touch {}; cat'", marker.display())]);
    sh(&env, &repo, &["config", "filter.evil=x.process", &format!("sh -c 'touch {}'", marker.display())]);
    sh(&env, &repo, &["config", "filter.evil=x.required", "true"]);
    std::fs::create_dir_all(repo.join(".git/info")).unwrap();
    std::fs::write(repo.join(".git/info/attributes"), "* filter=evil=x\n").unwrap();
    std::fs::write(repo.join(".gitattributes"), "*.md filter=evil=x\n").unwrap();
    std::fs::write(repo.join("notes.md"), "# Notes\n\none\nTWO\n").unwrap();
    let past = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
    std::fs::File::options().write(true).open(repo.join("notes.md")).unwrap().set_modified(past).unwrap();
    // Ember ran in the folder before this .git was recorded.
    let data = tmp.0.join("data");
    meerora_protocol::workspace::update_trust(&data, |t| {
        t.ran.insert(repo.clone(), "ember".into());
    })
    .unwrap();
    (repo.clone(), env, data, Areas { agents: Vec::new(), workspaces: vec![repo] })
}

#[test]
fn hardened_read_only_status_runs_no_planted_filter() {
    let tmp = TempDir::new("git-planted");
    let (repo, env, data, areas) = planted_repo(&tmp);
    let trust = meerora_protocol::workspace::Trust::load(&data);
    let git = repo::open(&repo, &areas, &trust, &env).unwrap();
    assert_eq!((git.mode, git.made_by.as_deref()), (Mode::Untrusted, Some("ember")));
    let status = git.status().unwrap();
    assert!(status.file("notes.md").is_some_and(|f| f.changed()), "the read-only status still works");
    assert!(!tmp.0.join("filter-ran").exists(), "the planted filter did not run");
    assert!(git.config_text().contains("filter \"evil=x\"") && git.config_text().contains(".gitattributes"));

    // A configuration that includes other files gets no git at all.
    sh(&env, &repo, &["config", "include.path", "../extra.conf"]);
    assert!(git.status().unwrap_err().contains("includes other files"));
    assert!(!tmp.0.join("filter-ran").exists());
}

#[test]
fn an_untrusted_repository_is_read_only_until_you_trust_it() {
    let tmp = TempDir::new("git-untrusted");
    let (repo, env, data, areas) = planted_repo(&tmp);
    let git = repo::open(&repo, &areas, &meerora_protocol::workspace::Trust::load(&data), &env).unwrap();
    assert_eq!(git.mode, Mode::Untrusted, "normal mode is refused");
    assert_eq!(git.stage("notes.md").unwrap_err(), repo::READ_ONLY);
    assert_eq!(git.commit("x", false, true).unwrap_err(), repo::READ_ONLY);
    assert!(git.push(None).is_err() && git.fetch().is_err() && git.discard("notes.md").is_err());
    assert!(repo::agent_act(&git, "stage", &serde_json::json!({ "path": "notes.md" })).is_err(), "agents cannot either");

    let mut inst = open_sync(git);
    inst.data_dir = data.clone();
    assert!(inst.read_only());
    assert_eq!(inst.commit_blocker(), Some("Read-only until you trust this repository."));
    inst.stage_file("notes.md".into());
    assert!(inst.action.is_none(), "nothing ran");
    assert!(!inst.file.as_ref().unwrap().editable, "no editing either");
    assert!(!tmp.0.join("filter-ran").exists());

    inst.trust();
    assert!(inst.reopen, "the app opens it again with your own git");
    let trust = meerora_protocol::workspace::Trust::load(&data);
    assert_eq!(trust.verdict(&repo.join(".git")), meerora_protocol::workspace::Verdict::Trusted);
    let git = repo::open(&repo, &areas, &trust, &env).unwrap();
    assert_eq!(git.mode, Mode::User, "trusted now, so your own git runs");
    // The setup does reach the filter: your own git runs it once you trust the repository.
    let _ = git.status();
    assert!(tmp.0.join("filter-ran").exists(), "a normal status hashes the change through the filter");
}
