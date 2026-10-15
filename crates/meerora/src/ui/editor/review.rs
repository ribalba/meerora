//! Review of the agent's changes (D12) on top of the git history of its
//! directory (D21).
//!
//! The state you last reviewed is a commit behind [`REVIEWED_REF`]. It
//! starts at HEAD the first time the editor sees the directory. Lines that
//! differ between that commit and the file on disk are the agent's changes.
//! Keep records the agent's version as reviewed, which makes a new commit on
//! top of the reviewed one and moves the ref. Revert writes the reviewed
//! version back to the file. Your own saved edits are folded into the
//! reviewed state, so they never show up as the agent's.
//!
//! git runs as a command with the user's configuration switched off, like
//! the agent daemon does it, and only plumbing commands touch the reviewed
//! state. The daemon's own index and branch are never changed.

use std::io::Write as _;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use similar::{Algorithm, DiffTag, MergeResolution, TextDiff, TextMerge};

/// The commit you last reviewed.
pub const REVIEWED_REF: &str = "refs/meerora/reviewed";

pub(crate) fn git(dir: &Path) -> Command {
    let mut cmd = Command::new("git");
    // The agent can write its directory. Nothing in it may run a command on
    // the host: no hooks, no fsmonitor, no attribute filters or textconv.
    cmd.env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_ATTR_SOURCE", "4b825dc642cb6eb9a060e54bf8d69288fbee4904")
        .env("LC_ALL", "C")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=meerora", "-c", "user.email=review@meerora.local"])
        .args(["-c", "commit.gpgsign=false", "-c", "core.hooksPath=/dev/null", "-c", "core.quotepath=false"])
        .args(["-c", "core.fsmonitor=false", "-c", "protocol.ext.allow=never"]);
    // Drivers named through .git/info/attributes would still run (gitsafe).
    let _ = meerora_protocol::gitsafe::neutralize(&mut cmd, dir);
    cmd
}

pub(crate) fn output(mut cmd: Command, what: &str) -> Result<Output, String> {
    let out = cmd.stdin(Stdio::null()).output().map_err(|e| format!("cannot run git ({e})"))?;
    if out.status.success() { Ok(out) } else { Err(format!("git {what} failed ({})", String::from_utf8_lossy(&out.stderr).trim())) }
}

pub(crate) fn text_of(out: Output) -> String {
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// `rel` in git's spelling.
fn git_path(rel: &Path) -> String {
    rel.to_string_lossy().replace('\\', "/")
}

/// An agent directory that is its own git repository with at least one commit.
#[derive(Clone, Debug)]
pub struct Repo {
    dir: PathBuf,
}

impl Repo {
    /// The directory as a repository, or why review is off. A directory
    /// inside another work tree (such as `assets/` of the meerora repository)
    /// does not count, its history is not the agent's.
    pub fn open(dir: &Path) -> Result<Repo, String> {
        let mut cmd = git(dir);
        cmd.args(["rev-parse", "--show-toplevel"]);
        let top = output(cmd, "rev-parse").map(text_of).map_err(|_| "the agent directory is not a git repository".to_string())?;
        let canonical = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
        if canonical(Path::new(&top)) != canonical(dir) {
            return Err(format!("the agent directory lies inside the work tree {top}"));
        }
        let repo = Repo { dir: dir.to_path_buf() };
        repo.rev("HEAD").ok_or("the agent directory has no commits yet")?;
        Ok(repo)
    }

    fn rev(&self, name: &str) -> Option<String> {
        let mut cmd = git(&self.dir);
        cmd.args(["rev-parse", "--verify", "-q", &format!("{name}^{{commit}}")]);
        output(cmd, "rev-parse").ok().map(text_of).filter(|s| !s.is_empty())
    }

    /// The reviewed commit. Starts it at HEAD when the ref does not exist.
    pub fn reviewed(&self) -> Result<String, String> {
        if let Some(id) = self.rev(REVIEWED_REF) {
            return Ok(id);
        }
        let head = self.rev("HEAD").ok_or("the agent directory has no commits yet")?;
        let mut cmd = git(&self.dir);
        cmd.args(["update-ref", "-m", "meerora: start the review", REVIEWED_REF, &head]);
        output(cmd, "update-ref")?;
        Ok(head)
    }

    /// The reviewed text of `rel`. `Ok(None)` when the file did not exist
    /// then, an error when it is not UTF-8.
    pub fn reviewed_text(&self, rel: &Path) -> Result<Option<String>, String> {
        let commit = self.reviewed()?;
        let mut child = git(&self.dir)
            .args(["cat-file", "--batch"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("cannot run git ({e})"))?;
        if let Some(mut stdin) = child.stdin.take() {
            let _ = writeln!(stdin, "{commit}:{}", git_path(rel));
        }
        let out = child.wait_with_output().map_err(|e| format!("git cat-file failed ({e})"))?;
        let bytes = out.stdout;
        let header_end = bytes.iter().position(|b| *b == b'\n').unwrap_or(bytes.len());
        let header = String::from_utf8_lossy(&bytes[..header_end]);
        if header.ends_with(" missing") || header.ends_with(" ambiguous") {
            return Ok(None);
        }
        let mut parts = header.split(' ');
        let (Some(_), Some(kind), Some(size)) = (parts.next(), parts.next(), parts.next()) else {
            return Err(format!("git cat-file printed {header:?}"));
        };
        if kind != "blob" {
            return Ok(None);
        }
        let size: usize = size.parse().map_err(|_| format!("git cat-file printed {header:?}"))?;
        let body = bytes.get(header_end + 1..header_end + 1 + size).ok_or("git cat-file stopped early")?;
        String::from_utf8(body.to_vec()).map(Some).map_err(|_| "the reviewed version is not UTF-8 text".into())
    }

    /// Files whose content differs from the reviewed state, new files
    /// included, relative to the directory. Ignored files (logs,
    /// artifacts) do not count.
    pub fn changed_files(&self) -> Result<Vec<PathBuf>, String> {
        let commit = self.reviewed()?;
        let mut cmd = git(&self.dir);
        cmd.args(["diff", "--no-ext-diff", "--name-only", "-z", &commit, "--"]);
        let tracked = output(cmd, "diff")?.stdout;
        let mut cmd = git(&self.dir);
        cmd.args(["ls-files", "--others", "--exclude-standard", "-z"]);
        let untracked = output(cmd, "ls-files")?.stdout;
        let mut files: Vec<PathBuf> = tracked
            .split(|b| *b == 0)
            .chain(untracked.split(|b| *b == 0))
            .filter(|p| !p.is_empty())
            .map(|p| PathBuf::from(String::from_utf8_lossy(p).into_owned()))
            .filter(|p| self.dir.join(p).is_file())
            .collect();
        files.sort();
        files.dedup();
        Ok(files)
    }

    /// Records `text` as the reviewed content of `rel` with a commit on top
    /// of the reviewed one, then moves the ref.
    pub fn set_reviewed(&self, rel: &Path, text: &str, message: &str) -> Result<(), String> {
        let old = self.reviewed()?;
        let path = git_path(rel);
        let mut child = git(&self.dir)
            .args(["hash-object", "-w", "--stdin"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("cannot run git ({e})"))?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(text.as_bytes()).map_err(|e| format!("git hash-object failed ({e})"))?;
        }
        let out = child.wait_with_output().map_err(|e| format!("git hash-object failed ({e})"))?;
        if !out.status.success() {
            return Err(format!("git hash-object failed ({})", String::from_utf8_lossy(&out.stderr).trim()));
        }
        let blob = text_of(out);

        let mut cmd = git(&self.dir);
        cmd.args(["ls-tree", &old, "--", &path]);
        let listed = text_of(output(cmd, "ls-tree")?);
        let mode = listed.split(' ').next().filter(|m| m.len() == 6).map(str::to_string).unwrap_or_else(|| self.mode_on_disk(rel));

        // A private index, so the daemon's index stays untouched.
        let index = std::env::temp_dir().join(format!("meerora-review-{}-{}.index", std::process::id(), unique()));
        let tree = (|| {
            let with_index = |args: &[&str]| {
                let mut cmd = git(&self.dir);
                cmd.env("GIT_INDEX_FILE", &index).args(args);
                cmd
            };
            output(with_index(&["read-tree", &old]), "read-tree")?;
            output(with_index(&["update-index", "--add", "--cacheinfo", &format!("{mode},{blob},{path}")]), "update-index")?;
            output(with_index(&["write-tree"]), "write-tree").map(text_of)
        })();
        let _ = std::fs::remove_file(&index);
        let tree = tree?;

        let mut cmd = git(&self.dir);
        cmd.args(["commit-tree", &tree, "-p", &old, "-m", message]);
        let commit = text_of(output(cmd, "commit-tree")?);
        let mut cmd = git(&self.dir);
        cmd.args(["update-ref", "-m", message, REVIEWED_REF, &commit, &old]);
        output(cmd, "update-ref")?;
        Ok(())
    }

    fn mode_on_disk(&self, rel: &Path) -> String {
        use std::os::unix::fs::PermissionsExt;
        let exec = std::fs::metadata(self.dir.join(rel)).is_ok_and(|m| m.permissions().mode() & 0o111 != 0);
        if exec { "100755" } else { "100644" }.to_string()
    }

    /// Subject of the newest commit that touched `rel`, which describes the
    /// change the editor tints. `None` while the file differs from HEAD,
    /// that is while a run is still changing it.
    pub fn change_subject(&self, rel: &Path) -> Option<String> {
        let path = git_path(rel);
        let mut cmd = git(&self.dir);
        cmd.args(["diff", "--no-ext-diff", "--quiet", "HEAD", "--", &path]);
        output(cmd, "diff").ok()?;
        let mut cmd = git(&self.dir);
        cmd.args(["log", "-1", "--format=%s", "HEAD", "--", &path]);
        output(cmd, "log").ok().map(text_of).filter(|s| !s.is_empty())
    }
}

fn unique() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// One change: lines `old` of the reviewed text became lines `new` of the
/// current text. Line indices start at 0, lines keep their line break.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hunk {
    pub old: Range<usize>,
    pub new: Range<usize>,
}

/// Lines with their line breaks, as the diff counts them.
pub fn lines(text: &str) -> Vec<&str> {
    text.split_inclusive('\n').collect()
}

fn diff<'a>(old: &'a str, new: &'a str) -> TextDiff<'a, 'a, str> {
    TextDiff::configure().algorithm(Algorithm::Myers).timeout(Duration::from_millis(300)).diff_lines(old, new)
}

/// The changes from `old` to `new`, neighbouring edits joined.
pub fn hunks(old: &str, new: &str) -> Vec<Hunk> {
    if old == new {
        return Vec::new();
    }
    let mut out: Vec<Hunk> = Vec::new();
    for op in diff(old, new).ops() {
        let (tag, o, n) = op.as_tag_tuple();
        if tag == DiffTag::Equal {
            continue;
        }
        match out.last_mut() {
            Some(last) if last.old.end == o.start && last.new.end == n.start => {
                last.old.end = o.end;
                last.new.end = n.end;
            }
            _ => out.push(Hunk { old: o, new: n }),
        }
    }
    out
}

/// The reviewed text with the agent's change `h` accepted.
pub fn keep_hunk(reviewed: &str, current: &str, h: &Hunk) -> String {
    let (r, c) = (lines(reviewed), lines(current));
    r[..h.old.start.min(r.len())].iter().chain(&c[h.new.clone()]).chain(&r[h.old.end.min(r.len())..]).copied().collect()
}

/// The current text with the agent's change `h` undone.
pub fn revert_hunk(reviewed: &str, current: &str, h: &Hunk) -> String {
    let (r, c) = (lines(reviewed), lines(current));
    c[..h.new.start.min(c.len())].iter().chain(&r[h.old.clone()]).chain(&c[h.new.end.min(c.len())..]).copied().collect()
}

/// For every line of `old`, the line of `new` it stayed as, if it stayed.
pub fn line_map(old: &str, new: &str) -> Vec<Option<usize>> {
    let mut map = vec![None; lines(old).len()];
    if old == new {
        return (0..map.len()).map(Some).collect();
    }
    for op in diff(old, new).ops() {
        let (tag, o, n) = op.as_tag_tuple();
        if tag == DiffTag::Equal {
            for (a, b) in o.zip(n) {
                map[a] = Some(b);
            }
        }
    }
    map
}

/// Your saved edit (`base` to `saved`) applied to the reviewed text. Where
/// your edit overlaps the agent's change, your lines win, since you
/// rewrote them yourself.
pub fn fold_edit(reviewed: &str, base: &str, saved: &str) -> String {
    let merge = TextMerge::from_lines(base, reviewed, saved);
    let mut out = String::with_capacity(saved.len());
    for region in merge.regions() {
        match region.resolution() {
            MergeResolution::Ours | MergeResolution::Unchanged => {
                for i in region.ours_range() {
                    out.push_str(merge.ours_line(i).unwrap_or_default());
                }
            }
            _ => {
                for i in region.theirs_range() {
                    out.push_str(merge.theirs_line(i).unwrap_or_default());
                }
            }
        }
    }
    out
}

/// Lines of the buffer to tint for one hunk. `lines` is empty for a hunk
/// that only removed lines, which shows as a marker above `lines.start`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tint {
    pub hunk: usize,
    pub lines: Range<usize>,
}

/// Where the agent's `hunks` (between the reviewed text and `base`) are in
/// `buffer`, which is `base` plus your unsaved edits. Lines you edited are
/// not tinted, they are yours.
pub fn tints(hunks: &[Hunk], base: &str, buffer: &str) -> Vec<Tint> {
    let map = line_map(base, buffer);
    let buffer_len = lines(buffer).len();
    let mut out = Vec::new();
    for (i, h) in hunks.iter().enumerate() {
        if h.new.is_empty() {
            let at = map.get(h.new.start).copied().flatten().unwrap_or_else(|| {
                // The line after the removal is gone or edited: put the marker after the last kept line before it.
                (0..h.new.start).rev().find_map(|l| map.get(l).copied().flatten()).map_or(0, |l| l + 1)
            });
            out.push(Tint { hunk: i, lines: at.min(buffer_len)..at.min(buffer_len) });
            continue;
        }
        let mut run: Option<Range<usize>> = None;
        for l in h.new.clone() {
            match (map.get(l).copied().flatten(), run.as_mut()) {
                (Some(b), Some(r)) if r.end == b => r.end = b + 1,
                (Some(b), _) => {
                    if let Some(r) = run.take() {
                        out.push(Tint { hunk: i, lines: r });
                    }
                    run = Some(b..b + 1);
                }
                (None, _) => {
                    if let Some(r) = run.take() {
                        out.push(Tint { hunk: i, lines: r });
                    }
                }
            }
        }
        if let Some(r) = run {
            out.push(Tint { hunk: i, lines: r });
        }
    }
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A directory under the system temp dir, removed again on drop.
    pub struct TempDir(pub PathBuf);

    impl TempDir {
        pub fn new(name: &str) -> Self {
            let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().subsec_nanos();
            let dir = std::env::temp_dir().join(format!("meerora-{name}-{}-{nanos}-{}", std::process::id(), unique()));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    pub fn run(dir: &Path, args: &[&str]) -> String {
        let mut cmd = git(dir);
        cmd.args(args);
        text_of(output(cmd, &args.join(" ")).unwrap())
    }

    /// A repository like the daemon makes it, with one run committed.
    pub fn agent_repo(name: &str) -> TempDir {
        let tmp = TempDir::new(name);
        run(&tmp.0, &["init", "-q", "-b", "main"]);
        std::fs::write(tmp.0.join(".gitignore"), "artifacts/\nlogs/\n").unwrap();
        std::fs::write(tmp.0.join("run.py"), "import time\n\ndef run():\n    return 1\n").unwrap();
        run(&tmp.0, &["add", "-A"]);
        run(&tmp.0, &["commit", "-q", "-m", "run 1: write run.py"]);
        tmp
    }

    #[test]
    fn hunks_cover_changed_lines() {
        let old = "a\nb\nc\nd\n";
        assert_eq!(hunks(old, old), vec![]);
        assert_eq!(hunks(old, "a\nB\nc\nd\n"), vec![Hunk { old: 1..2, new: 1..2 }]);
        assert_eq!(hunks(old, "a\nb\nx\ny\nc\nd\n"), vec![Hunk { old: 2..2, new: 2..4 }]);
        assert_eq!(hunks(old, "a\nd\n"), vec![Hunk { old: 1..3, new: 1..1 }]);
        assert_eq!(hunks(old, "A\nb\nc\nD\n"), vec![Hunk { old: 0..1, new: 0..1 }, Hunk { old: 3..4, new: 3..4 }]);
    }

    #[test]
    fn keep_and_revert_single_hunks() {
        let reviewed = "a\nb\nc\nd\n";
        let current = "A\nb\nc\nD\n";
        let hs = hunks(reviewed, current);
        assert_eq!(keep_hunk(reviewed, current, &hs[0]), "A\nb\nc\nd\n");
        assert_eq!(revert_hunk(reviewed, current, &hs[1]), "A\nb\nc\nd\n");
        let added = "a\nb\nnew\nc\nd\n";
        let h = &hunks(reviewed, added)[0];
        assert_eq!(keep_hunk(reviewed, added, h), added);
        assert_eq!(revert_hunk(reviewed, added, h), reviewed);
    }

    #[test]
    fn your_edits_are_not_tinted() {
        let base = "a\nB\nC\nd\n";
        let hs = hunks("a\nb\nc\nd\n", base);
        assert_eq!(tints(&hs, base, base), vec![Tint { hunk: 0, lines: 1..3 }]);
        // You inserted a line above and rewrote C.
        let buffer = "top\na\nB\nC2\nd\n";
        assert_eq!(tints(&hs, base, buffer), vec![Tint { hunk: 0, lines: 2..3 }]);
        // A removal shows as a marker where the lines were.
        let removed = hunks("a\nb\nc\n", "a\nc\n");
        assert_eq!(tints(&removed, "a\nc\n", "a\nc\n"), vec![Tint { hunk: 0, lines: 1..1 }]);
    }

    #[test]
    fn saved_edits_fold_into_the_review() {
        let reviewed = "a\nb\nc\nd\ne\n";
        let base = "a\nB\nc\nd\ne\n"; // the agent changed b
        let saved = "a\nB\nc\nd\nE\n"; // you changed e
        let folded = fold_edit(reviewed, base, saved);
        assert_eq!(folded, "a\nb\nc\nd\nE\n");
        assert_eq!(hunks(&folded, saved), vec![Hunk { old: 1..2, new: 1..2 }], "only the agent's line stays tinted");
        // You rewrote the agent's line yourself: it is yours now.
        assert_eq!(fold_edit(reviewed, base, "a\nmine\nc\nd\ne\n"), "a\nmine\nc\nd\ne\n");
    }

    #[test]
    fn tints_after_an_agent_commit_and_keep_moves_the_ref() {
        let repo_dir = agent_repo("review-keep");
        let repo = Repo::open(&repo_dir.0).unwrap();
        let start = repo.reviewed().unwrap();
        assert_eq!(start, run(&repo_dir.0, &["rev-parse", "HEAD"]), "the review starts at HEAD");

        // The agent's next run changes run.py and the daemon commits.
        let changed = "import time\n\ndef run():\n    time.sleep(1)\n    return 2\n";
        std::fs::write(repo_dir.0.join("run.py"), changed).unwrap();
        std::fs::write(repo_dir.0.join("notes.md"), "new\n").unwrap();
        std::fs::create_dir_all(repo_dir.0.join("logs")).unwrap();
        std::fs::write(repo_dir.0.join("logs/run.log"), "ignored\n").unwrap();
        run(&repo_dir.0, &["add", "-A"]);
        run(&repo_dir.0, &["commit", "-q", "-m", "run 2: sleep before returning"]);

        let reviewed = repo.reviewed_text(Path::new("run.py")).unwrap().unwrap();
        assert_eq!(reviewed, "import time\n\ndef run():\n    return 1\n");
        assert_eq!(hunks(&reviewed, changed), vec![Hunk { old: 3..4, new: 3..5 }], "lines 4 and 5 are tinted");
        assert_eq!(repo.reviewed_text(Path::new("notes.md")).unwrap(), None, "new since the review");
        assert_eq!(repo.changed_files().unwrap(), vec![PathBuf::from("notes.md"), PathBuf::from("run.py")]);
        assert_eq!(repo.change_subject(Path::new("run.py")).as_deref(), Some("run 2: sleep before returning"));

        repo.set_reviewed(Path::new("run.py"), changed, "meerora: keep run.py").unwrap();
        let moved = repo.reviewed().unwrap();
        assert_ne!(moved, start, "Keep moves the ref");
        assert_eq!(run(&repo_dir.0, &["rev-parse", &format!("{moved}^")]), start, "on top of the old review");
        assert_eq!(repo.reviewed_text(Path::new("run.py")).unwrap().as_deref(), Some(changed));
        assert_eq!(repo.changed_files().unwrap(), vec![PathBuf::from("notes.md")]);

        repo.set_reviewed(Path::new("notes.md"), "new\n", "meerora: keep notes.md").unwrap();
        assert!(repo.changed_files().unwrap().is_empty());
        assert_eq!(run(&repo_dir.0, &["status", "--porcelain"]), "", "the work tree and the daemon's index are untouched");
        assert_eq!(run(&repo_dir.0, &["log", "-1", "--format=%s"]), "run 2: sleep before returning", "HEAD stays");
    }

    #[test]
    fn revert_restores_the_reviewed_file() {
        let repo_dir = agent_repo("review-revert");
        let repo = Repo::open(&repo_dir.0).unwrap();
        repo.reviewed().unwrap();
        let path = repo_dir.0.join("run.py");
        let agent = "import os\n\ndef run():\n    return 1\n";
        std::fs::write(&path, agent).unwrap(); // a run still going, not committed yet
        let reviewed = repo.reviewed_text(Path::new("run.py")).unwrap().unwrap();
        let hs = hunks(&reviewed, agent);
        assert_eq!(hs, vec![Hunk { old: 0..1, new: 0..1 }]);
        assert_eq!(repo.change_subject(Path::new("run.py")), None, "not committed yet");

        std::fs::write(&path, revert_hunk(&reviewed, agent, &hs[0])).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), reviewed);
        assert!(repo.changed_files().unwrap().is_empty());
    }

    #[test]
    fn review_is_off_outside_own_repositories() {
        let tmp = TempDir::new("review-off");
        assert!(Repo::open(&tmp.0).unwrap_err().contains("not a git repository"));
        run(&tmp.0, &["init", "-q"]);
        assert!(Repo::open(&tmp.0).unwrap_err().contains("no commits"));
        let nested = tmp.0.join("agents/wren");
        std::fs::create_dir_all(&nested).unwrap();
        assert!(Repo::open(&nested).unwrap_err().contains("inside the work tree"));
    }
}
