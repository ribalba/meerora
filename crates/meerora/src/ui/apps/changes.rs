//! The Changes app of the agent view (D11, D12, D21). The agent daemon
//! commits the agent's directory after each run, so its git history is a
//! timeline of what every run changed. The newest entry comes first, with
//! its time, its task from the commit message, the files with their added
//! and removed lines and whether you reviewed it yet. Reviewed means the
//! run is part of `refs/meerora/reviewed` (Q2), or none of its files has a
//! change the review has not seen. Keep and your saved edits from the editor
//! are commits on that ref and show as yours. Like the editor, the app starts
//! the review at HEAD when the ref does not exist yet.
//!
//! Selecting an entry shows its diff. Undo reverts a run with `git revert`,
//! and your undo joins the reviewed state like your saved edits, so the
//! editor does not tint it as the agent's change. git runs on threads with
//! the editor's helper, so the UI never waits for it.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::Duration;

use egui::{Align2, Color32, CornerRadius, Id, Key, Rect, Sense, Stroke, StrokeKind, Ui, UiBuilder, pos2, vec2};
use similar::{Algorithm, ChangeTag, TextDiff};

use crate::engine::Core;
use crate::theme::{self, c, ca};
use crate::ui::editor::review::{self, REVIEWED_REF, Repo, git, output, text_of};
use crate::ui::{AppTab, ButtonStyle, button, fit, hline, pill, vline};

/// Seconds between checks whether HEAD or the review moved.
const POLL_EVERY: f64 = 1.5;
/// Seconds between tries while the directory has no history.
const POLL_OFF: f64 = 6.0;
/// Commits the timeline lists at most.
const MAX_COMMITS: usize = 300;
/// Diff lines kept per commit. Longer diffs are cut.
const MAX_DIFF_LINES: usize = 20_000;
/// The daemon commits runs as this author (meerora-agentd git.rs).
const AGENT_EMAIL: &str = "agents@meerora.local";
/// The editor's Keep, your edits and this app's undo commit as this author.
const REVIEW_EMAIL: &str = "review@meerora.local";
/// Subject of the review commits an undo makes. They only move the review
/// and stay out of the timeline.
const UNDO_FOLD: &str = "meerora: your undo of ";

const ROW_H: f32 = 74.0;
const FILE_ROW: f32 = 28.0;
const HEADER_H: f32 = 66.0;
const STRIP_H: f32 = 52.0;
const DIFF_HEAD: f32 = 36.0;
const DIFF_ROW: f32 = 19.0;
const DIFF_FONT: f32 = 12.0;

pub(super) const ADD: [u8; 3] = [0x7E, 0xC6, 0x8F];
pub(super) const DEL: [u8; 3] = [0xE0, 0x67, 0x5A];
const DIM: [u8; 3] = [0x8A, 0x92, 0xA3];
const LINE: [u8; 3] = [0x1C, 0x20, 0x29];
const BAR_BG: [u8; 3] = [0x0F, 0x12, 0x18];
const SELECTED: [u8; 3] = [0x1A, 0x1F, 0x2A];
const HOVERED: [u8; 3] = [0x16, 0x1A, 0x23];

// The history.

/// Who made a commit, from its author and subject.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A run of the agent, committed by the daemon.
    Run,
    /// The daemon's own commits, such as `meerora init`.
    Meerora,
    /// Kept in the editor, on the review ref (D12).
    Kept,
    /// Your saved edit in the editor, folded into the review (Q2).
    YourEdit,
    /// An undo from this app.
    Undo,
    /// Anyone else, for example a commit you made in the terminal.
    Other,
}

/// Lines a commit added and removed in one file. `None` for binary files.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileStat {
    pub path: String,
    pub added: Option<u32>,
    pub removed: Option<u32>,
}

/// One commit of the timeline.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub id: String,
    pub parents: Vec<String>,
    /// Unix seconds.
    pub time: i64,
    pub author: String,
    pub email: String,
    pub subject: String,
    pub body: String,
    pub kind: Kind,
    /// What the commit did in plain words: the task of a run, the file of a Keep.
    pub title: String,
    pub files: Vec<FileStat>,
    /// On the daemon's branch, so it can be undone. Keep and your edits
    /// live on the review ref only.
    pub on_head: bool,
    pub reviewed: bool,
    /// The undo commit that reverses this one.
    pub undone_by: Option<String>,
}

impl Entry {
    fn short(&self) -> &str {
        &self.id[..self.id.len().min(7)]
    }

    /// Whether Undo applies. It takes a commit with one parent on the
    /// daemon's branch that no undo reversed yet. The first commit made the
    /// agent and stays.
    pub fn undoable(&self) -> bool {
        self.on_head && self.parents.len() == 1 && self.undone_by.is_none()
    }

    fn totals(&self) -> (u32, u32) {
        self.files.iter().fold((0, 0), |(a, r), f| (a + f.added.unwrap_or(0), r + f.removed.unwrap_or(0)))
    }

    fn to_review(&self) -> bool {
        !self.reviewed && self.undone_by.is_none() && matches!(self.kind, Kind::Run | Kind::Other | Kind::Meerora)
    }
}

/// The timeline of an agent directory, newest first.
#[derive(Clone, Debug)]
pub struct History {
    pub entries: Vec<Entry>,
}

const LOG_FORMAT: &str = "--format=%x1e%H%x1f%P%x1f%at%x1f%an%x1f%ae%x1f%s%x1f%b";

/// Reads the timeline of `dir`: the daemon's branch and the review ref,
/// newest first. The error says why there is none, for example a
/// directory inside another work tree (as `Repo::open` checks it).
pub fn load_history(dir: &Path) -> Result<History, String> {
    let repo = Repo::open(dir)?;
    let reviewed = repo.reviewed()?;
    let head = rev(dir, "HEAD").ok_or("the agent directory has no commits yet")?;
    let mut cmd = git(dir);
    cmd.args(["log", "-z", "--no-renames", "--no-color", "--numstat", &format!("--max-count={MAX_COMMITS}"), LOG_FORMAT, "HEAD"])
        .arg(&reviewed)
        .arg("--");
    let mut entries = parse_log(&output(cmd, "log")?.stdout);
    entries.retain(|e| !(e.email == REVIEW_EMAIL && e.subject.starts_with(UNDO_FOLD)));

    let depth = MAX_COMMITS * 4;
    let on_head = rev_list(dir, "HEAD", depth)?;
    let in_review = rev_list(dir, &reviewed, depth)?;
    let mut cmd = git(dir);
    cmd.args(["diff", "--no-ext-diff", "--no-renames", "--name-only", "-z", &reviewed, &head, "--"]);
    let pending: HashSet<String> =
        output(cmd, "diff")?.stdout.split(|b| *b == 0).filter(|p| !p.is_empty()).map(|p| String::from_utf8_lossy(p).into_owned()).collect();
    let undone: HashMap<String, String> = entries
        .iter()
        .filter(|e| e.kind == Kind::Undo)
        .filter_map(|e| reverted_commit(&e.body).map(|target| (target, e.id.clone())))
        .collect();
    for e in &mut entries {
        e.on_head = on_head.contains(&e.id);
        e.reviewed = match e.kind {
            Kind::Kept | Kind::YourEdit => true,
            _ => in_review.contains(&e.id) || e.files.iter().all(|f| !pending.contains(&f.path)),
        };
        e.undone_by = undone.get(&e.id).cloned();
    }
    Ok(History { entries })
}

/// Parses `git log -z --numstat` in [`LOG_FORMAT`]: one record per commit
/// that starts with 0x1E, fields split by 0x1F and ended by NUL, then the
/// file stats, each ended by NUL.
pub fn parse_log(raw: &[u8]) -> Vec<Entry> {
    let text = String::from_utf8_lossy(raw);
    text.split('\u{1e}').filter(|r| !r.trim_matches(['\0', '\n']).is_empty()).filter_map(parse_record).collect()
}

fn parse_record(record: &str) -> Option<Entry> {
    let (header, stats) = record.split_once('\0').unwrap_or((record, ""));
    let mut fields = header.split('\u{1f}');
    let id = fields.next()?.trim().to_string();
    if id.len() < 7 {
        return None;
    }
    let parents = fields.next()?.split_whitespace().map(String::from).collect();
    let time = fields.next()?.trim().parse().ok()?;
    let author = fields.next()?.to_string();
    let email = fields.next()?.to_string();
    let subject = fields.next()?.to_string();
    let body = fields.next().unwrap_or_default().trim().to_string();
    let files = stats.split('\0').filter_map(parse_numstat).collect();
    let (kind, title) = classify(&email, &subject);
    Some(Entry { id, parents, time, author, email, subject, body, kind, title, files, on_head: false, reviewed: false, undone_by: None })
}

/// `added<TAB>removed<TAB>path`, with `-` for binary files.
fn parse_numstat(item: &str) -> Option<FileStat> {
    let item = item.trim_start_matches('\n');
    let mut parts = item.splitn(3, '\t');
    let (added, removed, path) = (parts.next()?, parts.next()?, parts.next()?);
    if path.is_empty() {
        return None;
    }
    Some(FileStat { path: path.to_string(), added: added.parse().ok(), removed: removed.parse().ok() })
}

/// The task of a daemon commit `run 2026-10-14 19:41: Fix the timeouts`.
fn run_task(subject: &str) -> Option<&str> {
    let rest = subject.strip_prefix("run ")?;
    rest.split_once(": ").map(|(_, task)| task.trim()).filter(|t| !t.is_empty())
}

fn classify(email: &str, subject: &str) -> (Kind, String) {
    if email == AGENT_EMAIL {
        return match run_task(subject) {
            Some(task) => (Kind::Run, task.to_string()),
            None => (Kind::Meerora, subject.to_string()),
        };
    }
    if email == REVIEW_EMAIL {
        if let Some(rest) = subject.strip_prefix("meerora: keep ") {
            return (Kind::Kept, format!("Kept {rest}"));
        }
        if let Some(rest) = subject.strip_prefix("meerora: your edit to ") {
            return (Kind::YourEdit, format!("Your edit to {rest}"));
        }
        if let Some(inner) = subject.strip_prefix("Revert \"").and_then(|s| s.strip_suffix('"')) {
            return (Kind::Undo, format!("Undid \"{}\"", run_task(inner).unwrap_or(inner)));
        }
    }
    (Kind::Other, subject.to_string())
}

/// The commit a `git revert` message names.
fn reverted_commit(body: &str) -> Option<String> {
    let rest = &body[body.find("This reverts commit ")? + "This reverts commit ".len()..];
    let id: String = rest.chars().take_while(|c| c.is_ascii_hexdigit()).collect();
    (id.len() >= 7).then_some(id)
}

fn rev(dir: &Path, name: &str) -> Option<String> {
    let mut cmd = git(dir);
    cmd.args(["rev-parse", "--verify", "-q", &format!("{name}^{{commit}}")]);
    output(cmd, "rev-parse").ok().map(text_of).filter(|s| !s.is_empty())
}

fn rev_list(dir: &Path, start: &str, max: usize) -> Result<HashSet<String>, String> {
    let mut cmd = git(dir);
    cmd.args(["rev-list", &format!("--max-count={max}"), start, "--"]);
    Ok(String::from_utf8_lossy(&output(cmd, "rev-list")?.stdout).lines().map(str::to_string).collect())
}

/// HEAD and the review, to notice when either moves.
fn heads(dir: &Path) -> Option<(String, String)> {
    Some((rev(dir, "HEAD")?, rev(dir, REVIEWED_REF)?))
}

// The diff of one commit.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineKind {
    /// `@@ -1,4 +1,5 @@`
    Hunk,
    Context,
    Added,
    Removed,
    /// `\ No newline at end of file` and similar notes.
    Note,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DiffLine {
    pub kind: LineKind,
    /// Without the leading `+`, `-` or space, tabs expanded.
    pub text: String,
    pub old: Option<u32>,
    pub new: Option<u32>,
    /// Byte ranges of `text` that differ from the paired removed or added
    /// line. They get a stronger tint than the rest of the line.
    pub changed: Vec<Range<usize>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileChange {
    Added,
    Deleted,
    Modified,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FileDiff {
    pub path: String,
    pub change: FileChange,
    pub binary: bool,
    pub lines: Vec<DiffLine>,
    /// The diff was too long and is cut.
    pub cut: bool,
}

/// A loaded diff, shared by the cache and the frame that draws it.
type DiffResult = Arc<Result<Vec<FileDiff>, String>>;

/// The unified diff of `commit` against its parent, file by file.
pub fn load_diff(dir: &Path, commit: &str) -> Result<Vec<FileDiff>, String> {
    let mut cmd = git(dir);
    cmd.args(["show", "--format=", "--no-color", "--no-ext-diff", "--no-renames", "-U3", commit, "--"]);
    Ok(parse_diff(&String::from_utf8_lossy(&output(cmd, "show")?.stdout)))
}

/// Parses a unified diff as `git show` prints it.
pub fn parse_diff(text: &str) -> Vec<FileDiff> {
    let mut files: Vec<FileDiff> = Vec::new();
    let (mut old, mut new, mut in_hunk, mut total) = (0u32, 0u32, false, 0usize);
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            if let Some(f) = files.last_mut() {
                mark_changes(&mut f.lines);
            }
            files.push(FileDiff { path: diff_path(rest), change: FileChange::Modified, binary: false, lines: Vec::new(), cut: false });
            in_hunk = false;
            continue;
        }
        let Some(file) = files.last_mut() else { continue };
        if line.starts_with("@@") {
            let (o, n) = hunk_start(line);
            (old, new, in_hunk) = (o, n, true);
            push_line(file, &mut total, DiffLine { kind: LineKind::Hunk, text: line.to_string(), old: None, new: None, changed: vec![] });
            continue;
        }
        if !in_hunk {
            if line.starts_with("new file mode") {
                file.change = FileChange::Added;
            } else if line.starts_with("deleted file mode") {
                file.change = FileChange::Deleted;
            } else if line.starts_with("Binary files ") || line == "GIT binary patch" {
                file.binary = true;
            }
            continue;
        }
        let (kind, rest) = match line.as_bytes().first() {
            Some(b'+') => (LineKind::Added, &line[1..]),
            Some(b'-') => (LineKind::Removed, &line[1..]),
            Some(b'\\') => (LineKind::Note, line.trim_start_matches('\\').trim()),
            Some(b' ') => (LineKind::Context, &line[1..]),
            _ => (LineKind::Context, line),
        };
        let (o, n) = match kind {
            LineKind::Added => (None, Some(new)),
            LineKind::Removed => (Some(old), None),
            LineKind::Context => (Some(old), Some(new)),
            _ => (None, None),
        };
        match kind {
            LineKind::Added => new += 1,
            LineKind::Removed => old += 1,
            LineKind::Context => {
                old += 1;
                new += 1;
            }
            _ => {}
        }
        push_line(file, &mut total, DiffLine { kind, text: rest.replace('\t', "    "), old: o, new: n, changed: vec![] });
    }
    if let Some(f) = files.last_mut() {
        mark_changes(&mut f.lines);
    }
    files
}

fn push_line(file: &mut FileDiff, total: &mut usize, line: DiffLine) {
    if *total >= MAX_DIFF_LINES {
        file.cut = true;
        return;
    }
    *total += 1;
    file.lines.push(line);
}

/// The path of `a/<path> b/<path>`. Without renames both halves are the same.
fn diff_path(rest: &str) -> String {
    let unquote = |s: &str| s.trim_matches('"').to_string();
    if rest.len() > 6 && (rest.len() - 3).is_multiple_of(2) {
        let half = (rest.len() - 3) / 2;
        if rest.is_char_boundary(half) && rest.is_char_boundary(half + 3) && &rest[half..half + 3] == " b/" {
            let a = &rest[..half];
            if let Some(path) = a.strip_prefix("a/") {
                return path.to_string();
            }
        }
    }
    match rest.rsplit_once(" b/") {
        Some((_, b)) => unquote(b),
        None => unquote(rest),
    }
}

/// The first old and new line numbers of `@@ -12,7 +12,9 @@`.
fn hunk_start(line: &str) -> (u32, u32) {
    let mut parts = line.split_whitespace().skip(1);
    let number = |p: Option<&str>, sign: char| -> u32 {
        p.and_then(|p| p.strip_prefix(sign)).and_then(|p| p.split(',').next()).and_then(|n| n.parse().ok()).unwrap_or(1)
    };
    (number(parts.next(), '-'), number(parts.next(), '+'))
}

/// Pairs each run of removed lines with the added lines right after it and
/// marks the bytes that changed between the pairs.
fn mark_changes(lines: &mut [DiffLine]) {
    let mut i = 0;
    while i < lines.len() {
        if lines[i].kind != LineKind::Removed {
            i += 1;
            continue;
        }
        let removed = i;
        while i < lines.len() && lines[i].kind == LineKind::Removed {
            i += 1;
        }
        let added = i;
        while i < lines.len() && lines[i].kind == LineKind::Added {
            i += 1;
        }
        let pairs = (added - removed).min(i - added);
        for k in 0..pairs {
            let (old, new) = changed_ranges(&lines[removed + k].text, &lines[added + k].text);
            lines[removed + k].changed = old;
            lines[added + k].changed = new;
        }
    }
}

/// Byte ranges of `old` and of `new` that differ, from a character diff.
/// Gaps of up to two bytes between changes are joined and ranges grow to
/// whole words. A line that changed almost completely gets no ranges, since
/// its tint already says it all.
pub fn changed_ranges(old: &str, new: &str) -> (Vec<Range<usize>>, Vec<Range<usize>>) {
    if old == new || old.len() > 2000 || new.len() > 2000 {
        return (Vec::new(), Vec::new());
    }
    let diff = TextDiff::configure().algorithm(Algorithm::Myers).timeout(Duration::from_millis(10)).diff_chars(old, new);
    let (mut at_old, mut at_new) = (0, 0);
    let (mut o, mut n): (Vec<Range<usize>>, Vec<Range<usize>>) = (Vec::new(), Vec::new());
    let join = |ranges: &mut Vec<Range<usize>>, r: Range<usize>| match ranges.last_mut() {
        Some(last) if r.start <= last.end + 2 => last.end = r.end,
        _ => ranges.push(r),
    };
    for change in diff.iter_all_changes() {
        let len = change.value().len();
        match change.tag() {
            ChangeTag::Equal => {
                at_old += len;
                at_new += len;
            }
            ChangeTag::Delete => {
                join(&mut o, at_old..at_old + len);
                at_old += len;
            }
            ChangeTag::Insert => {
                join(&mut n, at_new..at_new + len);
                at_new += len;
            }
        }
    }
    let (o, n) = (whole_words(old, o), whole_words(new, n));
    let share = |ranges: &[Range<usize>], text: &str| ranges.iter().map(|r| r.len()).sum::<usize>() as f32 / text.len().max(1) as f32;
    if share(&o, old) > 0.65 && share(&n, new) > 0.65 {
        return (Vec::new(), Vec::new());
    }
    (o, n)
}

/// Widens ranges that start or end inside a word to the whole word and
/// joins ranges that touch, so "Tuesday" to "Thursday" marks both words
/// instead of the letters in between.
fn whole_words(text: &str, ranges: Vec<Range<usize>>) -> Vec<Range<usize>> {
    let word = |c: char| c.is_alphanumeric() || c == '_';
    let before = |at: usize| text[..at].chars().next_back().is_some_and(word);
    let after = |at: usize| text[at..].chars().next().is_some_and(word);
    let mut out: Vec<Range<usize>> = Vec::with_capacity(ranges.len());
    for mut r in ranges {
        if after(r.start) && before(r.start) {
            r.start = text[..r.start].char_indices().rev().take_while(|(_, c)| word(*c)).last().map_or(r.start, |(i, _)| i);
        }
        if before(r.end) && after(r.end) {
            r.end = text[r.end..].char_indices().find(|(_, c)| !word(*c)).map_or(text.len(), |(i, _)| r.end + i);
        }
        match out.last_mut() {
            Some(last) if r.start <= last.end => last.end = last.end.max(r.end),
            _ => out.push(r),
        }
    }
    out
}

// Undo.

/// Reverts `commit` with `git revert --no-edit` in the agent's repository
/// and returns the new commit. When the revert conflicts it is aborted, so
/// the repository is left as it was, and the error names the files.
pub fn undo(dir: &Path, commit: &str) -> Result<String, String> {
    let repo = Repo::open(dir)?;
    let before = rev(dir, "HEAD").ok_or("the agent directory has no commits yet")?;
    let mut cmd = git(dir);
    cmd.args(["revert", "--no-edit", commit]);
    let out = cmd.stdin(Stdio::null()).output().map_err(|e| format!("cannot run git ({e})"))?;
    if out.status.success() {
        let after = rev(dir, "HEAD").ok_or("git revert left no HEAD")?;
        fold_into_review(dir, &repo, &before, &after);
        return Ok(after);
    }
    let mut cmd = git(dir);
    cmd.args(["diff", "--name-only", "-z", "--diff-filter=U"]);
    let conflicts: Vec<String> = output(cmd, "diff")
        .map(|o| o.stdout.split(|b| *b == 0).filter(|p| !p.is_empty()).map(|p| String::from_utf8_lossy(p).into_owned()).collect())
        .unwrap_or_default();
    if rev(dir, "REVERT_HEAD").is_some() {
        let mut cmd = git(dir);
        cmd.args(["revert", "--abort"]);
        if let Err(e) = output(cmd, "revert --abort") {
            return Err(format!("Undo stopped, and git could not clean up ({e}). Run git revert --abort in the agent's directory."));
        }
    }
    Err(undo_error(&String::from_utf8_lossy(&out.stdout), &String::from_utf8_lossy(&out.stderr), &conflicts))
}

/// A readable reason for a failed `git revert`.
fn undo_error(stdout: &str, stderr: &str, conflicts: &[String]) -> String {
    if !conflicts.is_empty() {
        return format!(
            "Undo stopped, because a later change touches the same lines in {}. The revert was aborted and nothing changed.",
            and_list(conflicts)
        );
    }
    if stdout.contains("nothing to commit") || stderr.contains("nothing to commit") {
        return "Nothing to undo, because later commits already reversed these changes.".into();
    }
    if stderr.contains("index.lock") {
        return "git is busy in the agent's directory. Try again in a moment.".into();
    }
    if stderr.contains("would be overwritten") {
        let files: Vec<String> = stderr.lines().filter(|l| l.starts_with('\t')).map(|l| l.trim().to_string()).collect();
        let what = if files.is_empty() { "files of this run".to_string() } else { and_list(&files) };
        return format!("Undo stopped, because {what} changed since the last commit. Nothing was changed.");
    }
    let reason = stderr
        .lines()
        .map(str::trim)
        .find(|l| l.starts_with("error:") || l.starts_with("fatal:"))
        .unwrap_or(stderr.trim())
        .trim_start_matches("error:")
        .trim_start_matches("fatal:")
        .trim();
    format!("Undo failed ({reason}).")
}

/// "a", "a and b", "a, b and c", at most three names.
fn and_list(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [a, b] => format!("{a} and {b}"),
        [a, b, c] => format!("{a}, {b} and {c}"),
        [a, b, rest @ ..] => format!("{a}, {b} and {} more files", rest.len()),
    }
}

/// Your undo joins the reviewed state like your saved edits do (Q2), so
/// the editor does not tint it as the agent's change. Where the review had
/// not seen the run yet, nothing changes.
fn fold_into_review(dir: &Path, repo: &Repo, before: &str, after: &str) {
    let mut cmd = git(dir);
    cmd.args(["diff", "--no-ext-diff", "--no-renames", "--name-only", "-z", before, after, "--"]);
    let Ok(out) = output(cmd, "diff") else { return };
    for path in out.stdout.split(|b| *b == 0).filter(|p| !p.is_empty()) {
        let rel = PathBuf::from(String::from_utf8_lossy(path).into_owned());
        let Some(now) = blob_text(dir, after, &rel) else { continue };
        let Ok(reviewed) = repo.reviewed_text(&rel) else { continue };
        let was = blob_text(dir, before, &rel).unwrap_or_default();
        let folded = review::fold_edit(reviewed.as_deref().unwrap_or(""), &was, &now);
        if reviewed.as_deref() != Some(folded.as_str())
            && let Err(e) = repo.set_reviewed(&rel, &folded, &format!("{UNDO_FOLD}{}", rel.display()))
        {
            tracing::warn!("the undo of {} did not join the review ({e})", rel.display());
        }
    }
}

/// The text of `rel` at `commit`, `None` when it is missing or not UTF-8.
fn blob_text(dir: &Path, commit: &str, rel: &Path) -> Option<String> {
    let mut cmd = git(dir);
    cmd.args(["cat-file", "blob", &format!("{commit}:{}", rel.to_string_lossy())]);
    String::from_utf8(output(cmd, "cat-file").ok()?.stdout).ok()
}

// Time for the apps of the agent view.

/// The local time zone and today, made once per frame.
pub(super) struct Clock {
    tz: jiff::tz::TimeZone,
    today: jiff::civil::Date,
    pub now_ms: i64,
}

impl Clock {
    pub(super) fn now() -> Self {
        let now = jiff::Zoned::now();
        Self { tz: now.time_zone().clone(), today: now.date(), now_ms: now.timestamp().as_millisecond() }
    }

    fn zoned(&self, ms: i64) -> Option<jiff::Zoned> {
        jiff::Timestamp::from_millisecond(ms).ok().map(|t| t.to_zoned(self.tz.clone()))
    }

    /// "19:41", "Yesterday 19:41", "Tue 13 Oct 19:41" or "13 Oct 2025".
    pub(super) fn when(&self, ms: i64) -> String {
        let Some(z) = self.zoned(ms) else { return String::new() };
        let date = z.date();
        if date == self.today {
            z.strftime("%H:%M").to_string()
        } else if self.today.yesterday().is_ok_and(|y| y == date) {
            z.strftime("Yesterday %H:%M").to_string()
        } else if date.year() == self.today.year() {
            z.strftime("%a %-d %b %H:%M").to_string()
        } else {
            z.strftime("%-d %b %Y").to_string()
        }
    }

    /// "19:41:05".
    pub(super) fn clock(&self, ms: i64) -> String {
        self.zoned(ms).map(|z| z.strftime("%H:%M:%S").to_string()).unwrap_or_default()
    }

    /// The local date as the daemon writes it for the token budget.
    pub(super) fn today(&self) -> String {
        self.today.to_string()
    }
}

/// "45 s", "3 min 12 s" or "2 h 05 min".
pub(super) fn duration(ms: u64) -> String {
    let s = ms / 1000;
    match s {
        0..60 => format!("{s} s"),
        60..3600 => format!("{} min {:02} s", s / 60, s % 60),
        _ => format!("{} h {:02} min", s / 3600, s % 3600 / 60),
    }
}

// The tile.

/// History, selection and pending git work of every agent's Changes app.
#[derive(Default)]
pub struct Changes {
    agents: HashMap<String, AgentChanges>,
}

type Job<T> = Receiver<T>;
/// The commit whose diff loads, and the diff when it is there.
type DiffJob = (String, Job<Result<Vec<FileDiff>, String>>);
/// What a poll found. `None` when nothing moved, else the history with the
/// HEAD and the review it was read at.
type Polled = Option<(Result<History, String>, Option<(String, String)>)>;

struct AgentChanges {
    dir: PathBuf,
    history: Option<Result<History, String>>,
    /// HEAD and the review the history was read at.
    known: Option<(String, String)>,
    poll: Option<Job<Polled>>,
    polled_at: f64,
    selected: Option<String>,
    file: usize,
    diffs: HashMap<String, DiffResult>,
    diff_job: Option<DiffJob>,
    /// The commit whose undo waits for your confirmation.
    confirm: Option<String>,
    undo: Option<(String, Job<Result<String, String>>)>,
    /// A line about the last undo: text, whether it is an error.
    note: Option<(String, bool)>,
    scroll_to_selected: bool,
}

/// What the agent view needs from a frame of the tile.
#[derive(Default)]
struct Out {
    /// "Open in editor": the file and the commit whose version opens.
    open: Option<(String, String)>,
    clicked: bool,
    focused: bool,
}

/// What the tile shows about the agent.
struct View {
    id: String,
    name: String,
    running: bool,
    selected: bool,
}

/// Draws the Changes tile of agent `index`. Returns true while it has keyboard focus.
pub fn tile(ui: &mut Ui, core: &mut Core, index: usize, head: Rect, body: Rect) -> bool {
    let a = &core.world.agents[index];
    let view = View { id: a.id.clone(), name: a.name.clone(), running: a.run.is_some(), selected: core.ui.tab == AppTab::Changes };
    let Some(dir) = core.agent_dir(&view.id).map(|d| d.path.clone()) else { return false };
    let now = core.world.time;
    let mut st = core.ui.changes.agents.remove(&view.id).filter(|s| s.dir == dir).unwrap_or_else(|| AgentChanges::new(dir));
    st.update(now);
    let mut out = Out::default();
    st.draw(ui, &view, head, body, &mut out);
    core.ui.changes.agents.insert(view.id.clone(), st);
    if out.clicked {
        core.ui.tab = AppTab::Changes;
    }
    if let Some((rel, commit)) = out.open {
        crate::ui::editor::open_revision(core, &view.id, Path::new(&rel), &commit);
    }
    out.focused
}

impl AgentChanges {
    fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            history: None,
            known: None,
            poll: None,
            polled_at: f64::NEG_INFINITY,
            selected: None,
            file: 0,
            diffs: HashMap::new(),
            diff_job: None,
            confirm: None,
            undo: None,
            note: None,
            scroll_to_selected: false,
        }
    }

    fn entries(&self) -> &[Entry] {
        match &self.history {
            Some(Ok(h)) => &h.entries,
            _ => &[],
        }
    }

    fn selected_entry(&self) -> Option<&Entry> {
        let id = self.selected.as_deref()?;
        self.entries().iter().find(|e| e.id == id)
    }

    /// Takes finished git work and starts new work.
    fn update(&mut self, now: f64) {
        if let Some(rx) = &self.poll {
            match rx.try_recv() {
                Ok(Some((history, known))) => {
                    self.poll = None;
                    self.set_history(history, known);
                }
                Ok(None) | Err(TryRecvError::Disconnected) => self.poll = None,
                Err(TryRecvError::Empty) => {}
            }
        }
        let every = if matches!(self.history, Some(Err(_))) { POLL_OFF } else { POLL_EVERY };
        if self.poll.is_none() && self.undo.is_none() && now - self.polled_at >= every {
            self.polled_at = now;
            self.poll = Some(start_poll(self.dir.clone(), self.known.clone()));
        }
        if let Some((commit, rx)) = &self.diff_job {
            match rx.try_recv() {
                Ok(diff) => {
                    if self.diffs.len() > 24 {
                        self.diffs.clear();
                    }
                    self.diffs.insert(commit.clone(), Arc::new(diff));
                    self.diff_job = None;
                }
                Err(TryRecvError::Disconnected) => self.diff_job = None,
                Err(TryRecvError::Empty) => {}
            }
        }
        if let Some((_, rx)) = &self.undo {
            match rx.try_recv() {
                Ok(Ok(commit)) => {
                    self.undo = None;
                    self.note = Some((format!("Undone. The new commit {} reverses those changes.", &commit[..commit.len().min(7)]), false));
                    self.selected = Some(commit);
                    self.file = 0;
                    self.scroll_to_selected = true;
                    self.refresh_now();
                }
                Ok(Err(e)) => {
                    self.undo = None;
                    self.note = Some((e, true));
                    self.refresh_now();
                }
                Err(TryRecvError::Disconnected) => self.undo = None,
                Err(TryRecvError::Empty) => {}
            }
        }
        if let Some(id) = self.selected.clone()
            && !self.diffs.contains_key(&id)
            && self.diff_job.as_ref().is_none_or(|(c, _)| *c != id)
            && self.entries().iter().any(|e| e.id == id)
        {
            self.diff_job = Some((id.clone(), start_diff(self.dir.clone(), id)));
        }
    }

    /// Reads the history again with the next frame, whether HEAD moved or not.
    fn refresh_now(&mut self) {
        self.known = None;
        self.polled_at = f64::NEG_INFINITY;
    }

    fn set_history(&mut self, history: Result<History, String>, known: Option<(String, String)>) {
        self.known = if history.is_ok() { known } else { None };
        self.history = Some(history);
        let entries = self.entries();
        let keep = self.selected.as_ref().is_some_and(|id| entries.iter().any(|e| e.id == *id));
        if !keep {
            // The newest run you have not reviewed, else the newest entry.
            let pick = entries.iter().find(|e| e.to_review()).or(entries.first()).map(|e| e.id.clone());
            self.selected = pick;
            self.file = 0;
        }
        if self.confirm.as_ref().is_some_and(|id| !self.entries().iter().any(|e| e.id == *id && e.undoable())) {
            self.confirm = None;
        }
    }

    fn select(&mut self, id: String) {
        if self.selected.as_deref() != Some(id.as_str()) {
            self.selected = Some(id);
            self.file = 0;
            self.confirm = None;
            self.note = None;
        }
    }

    fn draw(&mut self, ui: &mut Ui, v: &View, head: Rect, body: Rect, out: &mut Out) {
        if ui.rect_contains_pointer(body) && ui.input(|i| i.pointer.any_pressed()) {
            out.clicked = true;
        }
        let history = match &self.history {
            None => {
                super::super::agent::placeholder(ui, body, "Reading the history", "One moment.");
                return;
            }
            Some(Err(reason)) => {
                let (title, text) = no_history(&v.name, reason);
                super::super::agent::placeholder(ui, body, title, &text);
                return;
            }
            Some(Ok(h)) => h,
        };
        if history.entries.is_empty() {
            super::super::agent::placeholder(ui, body, "No commits yet", "The agent daemon commits the directory after each run.");
            return;
        }
        let waiting = history.entries.iter().filter(|e| e.to_review()).count();
        let (text, color) = match waiting {
            0 => ("ALL REVIEWED".to_string(), [0xB9, 0xBF, 0xCC]),
            1 => ("1 RUN TO REVIEW".to_string(), theme::NEEDS_YOU),
            n => (format!("{n} RUNS TO REVIEW"), theme::NEEDS_YOU),
        };
        if head.width() > 160.0 {
            pill(ui.painter(), pos2(head.max.x - 8.0, head.center().y), &text, color);
        }

        self.keys(ui, v, out);
        let clock = Clock::now();
        let wide = body.width() >= 760.0;
        let (list_r, detail_r) = if wide {
            let w = (body.width() * 0.34).clamp(270.0, 360.0).floor();
            (Rect::from_min_size(body.min, vec2(w, body.height())), Rect::from_min_max(pos2(body.min.x + w, body.min.y), body.max))
        } else {
            let h = (body.height() * 0.4).clamp(150.0, 340.0).floor();
            (Rect::from_min_size(body.min, vec2(body.width(), h)), Rect::from_min_max(pos2(body.min.x, body.min.y + h), body.max))
        };
        let painter = ui.painter_at(body);
        painter.rect_filled(list_r, CornerRadius::ZERO, c(BAR_BG));
        if wide {
            vline(&painter, list_r.max.x - 0.5, list_r.min.y, list_r.max.y);
        } else {
            hline(&painter, list_r.max.y - 0.5, list_r.min.x, list_r.max.x);
        }
        if let Some(id) = self.draw_timeline(ui, v, list_r, &clock) {
            self.select(id);
        }
        self.draw_detail(ui, v, detail_r, &clock, out);
    }

    /// Up and Down move through the timeline while the tile is selected and
    /// nothing else takes the keys. Esc cancels a pending undo.
    fn keys(&mut self, ui: &mut Ui, v: &View, out: &mut Out) {
        if self.confirm.is_some() {
            out.focused = true;
            if ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Escape)) {
                self.confirm = None;
            }
        }
        if !v.selected || ui.memory(|m| m.focused().is_some()) {
            return;
        }
        let (up, down) = ui.input(|i| (i.key_pressed(Key::ArrowUp), i.key_pressed(Key::ArrowDown)));
        if !(up || down) {
            return;
        }
        let entries = self.entries();
        let at = self.selected.as_ref().and_then(|id| entries.iter().position(|e| e.id == *id)).unwrap_or(0);
        let next = if down { (at + 1).min(entries.len().saturating_sub(1)) } else { at.saturating_sub(1) };
        if let Some(id) = entries.get(next).map(|e| e.id.clone()) {
            self.select(id);
            self.scroll_to_selected = true;
        }
    }

    /// The list of commits. Returns the entry clicked.
    fn draw_timeline(&mut self, ui: &mut Ui, v: &View, r: Rect, clock: &Clock) -> Option<String> {
        let entries = self.entries();
        let n = entries.len();
        let selected = self.selected.clone();
        let target = self.scroll_to_selected.then(|| entries.iter().position(|e| Some(&e.id) == selected.as_ref())).flatten();
        let mut clicked = None;
        let mut child = ui.new_child(UiBuilder::new().max_rect(r).layout(egui::Layout::top_down(egui::Align::Min)));
        child.set_clip_rect(r.intersect(ui.clip_rect()));
        egui::ScrollArea::vertical()
            .id_salt(("changes-list", &v.id))
            .auto_shrink([false, false])
            .content_margin(egui::Margin::ZERO)
            .show_viewport(&mut child, |ui, viewport| {
                ui.spacing_mut().item_spacing = egui::Vec2::ZERO;
                let width = r.width();
                let (all, _) = ui.allocate_exact_size(vec2(width, ROW_H * n as f32 + 8.0), Sense::hover());
                let row_at = |i: usize| Rect::from_min_size(pos2(all.min.x, all.min.y + 4.0 + i as f32 * ROW_H), vec2(width, ROW_H));
                if let Some(i) = target {
                    ui.scroll_to_rect(row_at(i), None);
                }
                let first = ((viewport.min.y - 4.0) / ROW_H).floor().max(0.0) as usize;
                let last = (((viewport.max.y - 4.0) / ROW_H).ceil().max(0.0) as usize + 1).min(n);
                for (i, e) in entries.iter().enumerate().take(last).skip(first) {
                    let row = row_at(i);
                    let resp = ui.interact(row, Id::new(("changes-row", &v.id, &e.id)), Sense::click());
                    if resp.clicked() {
                        clicked = Some(e.id.clone());
                    }
                    if resp.hovered() {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                    }
                    let is_selected = selected.as_deref() == Some(e.id.as_str());
                    timeline_row(ui, e, row, is_selected, resp.hovered(), i == 0, i + 1 == n, clock);
                }
            });
        self.scroll_to_selected = false;
        clicked
    }

    fn draw_detail(&mut self, ui: &mut Ui, v: &View, r: Rect, clock: &Clock, out: &mut Out) {
        let Some(entry) = self.selected_entry().cloned() else {
            super::super::agent::placeholder(ui, r, "Nothing selected", "Pick an entry on the left.");
            return;
        };
        let x = r.min.x + 16.0;
        let right = r.max.x - 14.0;
        let painter = ui.painter_at(r);

        // Undo on the right of the header.
        let mut buttons_left = right;
        if entry.undoable() {
            let label = if entry.kind == Kind::Run { "Undo this run" } else { "Undo this commit" };
            let b = Rect::from_min_size(pos2(right - 136.0, r.min.y + 18.0), vec2(136.0, 30.0));
            buttons_left = b.min.x - 12.0;
            if self.undo.as_ref().is_some_and(|(id, _)| *id == entry.id) {
                dim_button(ui, b, "Undoing…", Id::new(("changes-undo", &v.id)));
            } else if v.running || self.undo.is_some() {
                dim_button(ui, b, label, Id::new(("changes-undo", &v.id)))
                    .on_hover_text(format!("Wait until {}'s run has ended. Undo changes files the run may be using.", v.name));
            } else if self.confirm.as_deref() == Some(entry.id.as_str()) {
                button(ui, b, label, ButtonStyle::Selected);
            } else if button(ui, b, label, ButtonStyle::Quiet).clicked() {
                self.confirm = Some(entry.id.clone());
                self.note = None;
            }
        }
        let title_color = if entry.undone_by.is_some() { DIM } else { theme::TEXT };
        let title = fit(&painter, &entry.title, theme::ui_semibold(15.5), buttons_left - x);
        painter.text(pos2(x, r.min.y + 14.0), Align2::LEFT_TOP, title, theme::ui_semibold(15.5), c(title_color));
        let mut meta = format!("{} · {} · {}", who(&entry, &v.name), clock.when(entry.time * 1000), entry.short());
        if let Some(undo) = &entry.undone_by {
            meta.push_str(&format!(" · undone by {}", &undo[..undo.len().min(7)]));
        }
        let meta = fit(&painter, &meta, theme::mono(11.0), buttons_left - x);
        painter.text(pos2(x, r.min.y + 40.0), Align2::LEFT_TOP, meta, theme::mono(11.0), c(theme::TEXT_2));
        let mut top = r.min.y + HEADER_H;

        // The confirmation, or what the last undo did.
        if self.confirm.as_deref() == Some(entry.id.as_str()) {
            let strip = Rect::from_min_max(pos2(r.min.x + 12.0, top), pos2(r.max.x - 12.0, top + STRIP_H - 8.0));
            self.draw_confirm(ui, v, &entry, strip);
            top += STRIP_H;
        } else if let Some((text, error)) = &self.note {
            let strip = Rect::from_min_max(pos2(r.min.x + 12.0, top), pos2(r.max.x - 12.0, top + STRIP_H - 8.0));
            let color = if *error { DEL } else { ADD };
            let painter = ui.painter_at(r);
            painter.rect(strip, CornerRadius::same(10), ca(color, 18), Stroke::new(1.0, ca(color, 80)), StrokeKind::Inside);
            let galley = painter.layout(text.clone(), theme::ui(13.0), c(theme::TEXT), strip.width() - 24.0);
            painter.galley(pos2(strip.min.x + 12.0, strip.center().y - galley.size().y / 2.0), galley, c(theme::TEXT));
            top += STRIP_H;
        }
        hline(&ui.painter_at(r), top - 0.5, r.min.x, r.max.x);
        let area = Rect::from_min_max(pos2(r.min.x, top), r.max);
        if area.height() < 40.0 {
            return;
        }
        if entry.files.is_empty() {
            super::super::agent::placeholder(ui, area, "No file changes", "This commit changed no files.");
            return;
        }
        self.file = self.file.min(entry.files.len() - 1);
        let diff = self.diffs.get(&entry.id).cloned();
        let side = area.width() >= 640.0;
        let (files_r, diff_r) = if side {
            let w = (area.width() * 0.3).clamp(190.0, 270.0).floor();
            (Rect::from_min_size(area.min, vec2(w, area.height())), Rect::from_min_max(pos2(area.min.x + w, area.min.y), area.max))
        } else {
            let h = (entry.files.len() as f32 * FILE_ROW + 12.0).min((area.height() * 0.32).max(FILE_ROW + 12.0)).floor();
            (Rect::from_min_size(area.min, vec2(area.width(), h)), Rect::from_min_max(pos2(area.min.x, area.min.y + h), area.max))
        };
        let painter = ui.painter_at(area);
        if side {
            vline(&painter, files_r.max.x - 0.5, files_r.min.y, files_r.max.y);
        } else {
            hline(&painter, files_r.max.y - 0.5, files_r.min.x, files_r.max.x);
        }
        if let Some(i) = self.draw_files(ui, v, &entry, diff.as_deref(), files_r) {
            self.file = i;
        }
        let stat = &entry.files[self.file];
        let file_diff = match diff.as_deref() {
            Some(Ok(files)) => Some(Ok(files.iter().find(|f| f.path == stat.path))),
            Some(Err(e)) => Some(Err(e.clone())),
            None => None,
        };
        self.draw_diff(ui, v, &entry, stat, file_diff, diff_r, out);
    }

    fn draw_confirm(&mut self, ui: &mut Ui, v: &View, entry: &Entry, strip: Rect) {
        let painter = ui.painter_at(strip.expand(2.0));
        painter.rect(
            strip,
            CornerRadius::same(10),
            ca(theme::NEEDS_YOU, 16),
            Stroke::new(1.0, ca(theme::NEEDS_YOU, 70)),
            StrokeKind::Inside,
        );
        let undo_b = Rect::from_min_size(pos2(strip.max.x - 10.0 - 84.0, strip.center().y - 15.0), vec2(84.0, 30.0));
        let cancel_b = Rect::from_min_size(pos2(undo_b.min.x - 8.0 - 76.0, undo_b.min.y), vec2(76.0, 30.0));
        let files = match entry.files.len() {
            1 => "1 file".to_string(),
            n => format!("{n} files"),
        };
        let text = format!("Undo it? A new commit reverses its changes to {files}, and you can undo that as well.");
        let galley = painter.layout(text, theme::ui(13.0), c(theme::TEXT), (cancel_b.min.x - strip.min.x - 24.0).max(40.0));
        painter.galley(pos2(strip.min.x + 12.0, strip.center().y - galley.size().y / 2.0), galley, c(theme::TEXT));
        if button(ui, cancel_b, "Cancel", ButtonStyle::Quiet).clicked() {
            self.confirm = None;
        }
        if v.running {
            dim_button(ui, undo_b, "Undo", Id::new(("changes-confirm", &v.id))).on_hover_text("A run started. Wait until it has ended.");
        } else if button(ui, undo_b, "Undo", ButtonStyle::Primary).clicked() {
            self.confirm = None;
            self.note = None;
            // A poll from before the undo would bring back the old history.
            self.poll = None;
            self.undo = Some((entry.id.clone(), start_undo(self.dir.clone(), entry.id.clone())));
        }
    }

    /// The files of the selected commit. Returns the one clicked.
    fn draw_files(&self, ui: &mut Ui, v: &View, entry: &Entry, diff: Option<&Result<Vec<FileDiff>, String>>, r: Rect) -> Option<usize> {
        let mut clicked = None;
        let mut child = ui.new_child(UiBuilder::new().max_rect(r.shrink2(vec2(0.0, 6.0))).layout(egui::Layout::top_down(egui::Align::Min)));
        child.set_clip_rect(r.intersect(ui.clip_rect()));
        egui::ScrollArea::vertical().id_salt(("changes-files", &v.id, &entry.id)).auto_shrink([false, false]).show(&mut child, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            for (i, f) in entry.files.iter().enumerate() {
                let (row, resp) = ui.allocate_exact_size(vec2(r.width(), FILE_ROW), Sense::click());
                if !ui.is_rect_visible(row) {
                    continue;
                }
                let painter = ui.painter();
                if i == self.file {
                    painter.rect_filled(row.shrink2(vec2(6.0, 1.0)), CornerRadius::same(6), c(SELECTED));
                } else if resp.hovered() {
                    painter.rect_filled(row.shrink2(vec2(6.0, 1.0)), CornerRadius::same(6), c(HOVERED));
                }
                let change = match diff {
                    Some(Ok(files)) => files.iter().find(|d| d.path == f.path).map(|d| d.change),
                    _ => None,
                };
                let (letter, color) = match change {
                    Some(FileChange::Added) => ("A", ADD),
                    Some(FileChange::Deleted) => ("D", DEL),
                    Some(FileChange::Modified) => ("M", DIM),
                    None => ("", DIM),
                };
                painter.text(pos2(row.min.x + 16.0, row.center().y), Align2::LEFT_CENTER, letter, theme::mono_medium(11.0), c(color));
                let counts = match (f.added, f.removed) {
                    (Some(a), Some(r)) => format!("+{a} −{r}"),
                    _ => "binary".to_string(),
                };
                let counts_r = painter.text(
                    pos2(row.max.x - 12.0, row.center().y),
                    Align2::RIGHT_CENTER,
                    &counts,
                    theme::mono(10.5),
                    c(theme::TEXT_2),
                );
                let room = counts_r.min.x - row.min.x - 40.0;
                let (dir, name) = match f.path.rsplit_once('/') {
                    Some((d, n)) => (format!("{d}/"), n.to_string()),
                    None => (String::new(), f.path.clone()),
                };
                let name_w = painter.layout_no_wrap(name.clone(), theme::mono(12.0), Color32::WHITE).size().x;
                let name_color = if i == self.file { c(theme::TEXT) } else { c([0xC9, 0xCE, 0xD9]) };
                if dir.is_empty() || name_w >= room - 24.0 {
                    let shown = fit(painter, &f.path, theme::mono(12.0), room);
                    painter.text(pos2(row.min.x + 32.0, row.center().y), Align2::LEFT_CENTER, shown, theme::mono(12.0), name_color);
                } else {
                    let dir = fit(painter, &dir, theme::mono(12.0), room - name_w);
                    let d = painter.text(pos2(row.min.x + 32.0, row.center().y), Align2::LEFT_CENTER, dir, theme::mono(12.0), c(DIM));
                    painter.text(pos2(d.max.x, row.center().y), Align2::LEFT_CENTER, name, theme::mono(12.0), name_color);
                }
                if resp.clicked() {
                    clicked = Some(i);
                }
                if resp.hovered() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                }
            }
        });
        clicked
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_diff(
        &self,
        ui: &mut Ui,
        v: &View,
        entry: &Entry,
        stat: &FileStat,
        diff: Option<Result<Option<&FileDiff>, String>>,
        r: Rect,
        out: &mut Out,
    ) {
        if r.width() < 60.0 || r.height() < DIFF_HEAD + 20.0 {
            return;
        }
        let painter = ui.painter_at(r);
        let head = Rect::from_min_size(r.min, vec2(r.width(), DIFF_HEAD));
        hline(&painter, head.max.y - 0.5, r.min.x, r.max.x);
        let b = Rect::from_min_size(pos2(head.max.x - 12.0 - 126.0, head.center().y - 13.0), vec2(126.0, 26.0));
        // The file as this change left it, read-only, also when it is gone by now.
        if button(ui, b, "Open in editor", ButtonStyle::Quiet).on_hover_text("Opens the file as this change left it").clicked() {
            out.open = Some((stat.path.clone(), entry.id.clone()));
        }
        let path = fit(&painter, &stat.path, theme::mono(12.0), b.min.x - head.min.x - 28.0);
        painter.text(pos2(head.min.x + 14.0, head.center().y), Align2::LEFT_CENTER, path, theme::mono(12.0), c(theme::TEXT));
        let body = Rect::from_min_max(pos2(r.min.x, head.max.y), r.max);
        let file = match diff {
            None => {
                super::super::agent::placeholder(ui, body, "Reading the diff", "One moment.");
                return;
            }
            Some(Err(e)) => {
                super::super::agent::placeholder(ui, body, "The diff could not be read", &e);
                return;
            }
            Some(Ok(None)) => {
                super::super::agent::placeholder(ui, body, "No text changes", "Only the file mode changed.");
                return;
            }
            Some(Ok(Some(file))) if file.binary => {
                super::super::agent::placeholder(ui, body, "Binary file", "Its changes are not shown.");
                return;
            }
            Some(Ok(Some(file))) => file,
        };
        diff_view(ui, &v.id, &entry.id, file, body);
    }
}

/// One entry of the timeline: a dot on the rail, the time and a mark, the
/// title and the file stats.
#[allow(clippy::too_many_arguments)]
fn timeline_row(ui: &Ui, e: &Entry, row: Rect, selected: bool, hovered: bool, first: bool, last: bool, clock: &Clock) {
    let painter = ui.painter();
    if selected {
        painter.rect_filled(row.shrink2(vec2(6.0, 2.0)), CornerRadius::same(10), c(SELECTED));
    } else if hovered {
        painter.rect_filled(row.shrink2(vec2(6.0, 2.0)), CornerRadius::same(10), c(HOVERED));
    }
    let rail_x = row.min.x + 24.0;
    let dot = pos2(rail_x, row.min.y + 19.0);
    let y0 = if first { dot.y } else { row.min.y };
    let y1 = if last { dot.y } else { row.max.y };
    painter.line_segment([pos2(rail_x, y0), pos2(rail_x, y1)], Stroke::new(1.5, c([0x2A, 0x30, 0x3D])));
    // Blue for a run waiting for your review (D3), hollow for an undone run
    // and for your own commits.
    if e.undone_by.is_some() {
        painter.circle(dot, 4.5, c(BAR_BG), Stroke::new(1.5, c(DIM)));
    } else if e.to_review() {
        painter.circle_filled(dot, 5.5, c(theme::NEEDS_YOU));
    } else if matches!(e.kind, Kind::Run | Kind::Meerora) {
        painter.circle_filled(dot, 4.5, c(DIM));
    } else {
        painter.circle(dot, 4.5, c(BAR_BG), Stroke::new(1.5, c(theme::TEXT)));
    }
    let x = row.min.x + 44.0;
    let right = row.max.x - 14.0;
    let (mark_text, mark_color) = mark(e);
    let mark_r = pill(painter, pos2(right, row.min.y + 19.0), &mark_text, mark_color);
    let when = fit(painter, &clock.when(e.time * 1000), theme::mono(10.5), mark_r.min.x - x - 8.0);
    painter.text(pos2(x, row.min.y + 19.0), Align2::LEFT_CENTER, when, theme::mono(10.5), c(theme::TEXT_2));
    let title_color = if e.undone_by.is_some() { c(DIM) } else { c(theme::TEXT) };
    let title = fit(painter, &e.title, theme::ui(14.0), right - x);
    painter.text(pos2(x, row.min.y + 39.0), Align2::LEFT_CENTER, title, theme::ui(14.0), title_color);
    let y = row.min.y + 58.0;
    let files = match e.files.len() {
        0 => "NO FILES".to_string(),
        1 => "1 FILE".to_string(),
        n => format!("{n} FILES"),
    };
    let r = painter.text(pos2(x, y), Align2::LEFT_CENTER, files, theme::mono(10.5), c(theme::TEXT_2));
    if !e.files.is_empty() {
        let (added, removed) = e.totals();
        let r = painter.text(pos2(r.max.x + 10.0, y), Align2::LEFT_CENTER, format!("+{added}"), theme::mono(10.5), c(ADD));
        painter.text(pos2(r.max.x + 6.0, y), Align2::LEFT_CENTER, format!("−{removed}"), theme::mono(10.5), c(DEL));
    }
}

/// The pill of an entry, which is its review state or who made it.
fn mark(e: &Entry) -> (String, [u8; 3]) {
    if e.undone_by.is_some() {
        return ("UNDONE".into(), DIM);
    }
    match e.kind {
        Kind::Kept => ("KEPT".into(), [0xB9, 0xBF, 0xCC]),
        Kind::YourEdit => ("YOUR EDIT".into(), [0xB9, 0xBF, 0xCC]),
        Kind::Undo => ("YOUR UNDO".into(), [0xB9, 0xBF, 0xCC]),
        _ if e.to_review() => ("TO REVIEW".into(), theme::NEEDS_YOU),
        Kind::Other => (format!("BY {}", e.author.to_uppercase()), [0xB9, 0xBF, 0xCC]),
        _ => ("REVIEWED".into(), DIM),
    }
}

/// Who made the entry, for the detail header.
fn who(e: &Entry, name: &str) -> String {
    match e.kind {
        Kind::Run => format!("Run by {name}"),
        Kind::Meerora => "By meerora".into(),
        Kind::Kept => "Kept by you in the editor".into(),
        Kind::YourEdit => "Your edit in the editor".into(),
        Kind::Undo => "Your undo".into(),
        Kind::Other => format!("By {}", e.author),
    }
}

/// Title and text of the note shown when the directory has no history.
fn no_history(name: &str, reason: &str) -> (&'static str, String) {
    if let Some(top) = reason.split("inside the work tree ").nth(1) {
        return (
            "No history of its own",
            format!(
                "{name}'s directory lies inside the git work tree {top}, so it has no history of its own. Agents get their own \
                 repository in a data directory made with meerora-agentd init."
            ),
        );
    }
    if reason.contains("not a git repository") {
        return ("No history yet", format!("{name}'s directory is not a git repository yet. The agent daemon makes it one."));
    }
    if reason.contains("no commits") {
        return ("No history yet", format!("{name}'s directory has no commits yet. The agent daemon commits it after each run."));
    }
    ("No history", format!("The history of {name}'s directory could not be read ({reason})."))
}

/// A button that does nothing, drawn dim, for actions that wait.
fn dim_button(ui: &mut Ui, rect: Rect, label: &str, id: Id) -> egui::Response {
    let resp = ui.interact(rect, id, Sense::hover());
    let painter = ui.painter();
    painter.rect(rect, CornerRadius::same(10), Color32::TRANSPARENT, Stroke::new(1.0, c([0x22, 0x27, 0x31])), StrokeKind::Inside);
    painter.text(rect.center(), Align2::CENTER_CENTER, label, theme::ui(14.5), c([0x5A, 0x60, 0x6E]));
    resp
}

/// The coloured unified diff of one file. Only the rows in view are drawn,
/// and the line numbers stay put while the text scrolls sideways.
fn diff_view(ui: &mut Ui, agent: &str, commit: &str, file: &FileDiff, r: Rect) {
    let font = theme::mono(DIFF_FONT);
    let cw = ui.fonts_mut(|f| f.glyph_width(&font, '0')).max(1.0);
    let rows = file.lines.len() + usize::from(file.cut);
    let widest = file.lines.iter().map(|l| l.text.chars().count()).max().unwrap_or(0);
    let digits = file.lines.iter().filter_map(|l| l.new.max(l.old)).max().unwrap_or(1).to_string().len().max(3) as f32;
    let num_w = digits * cw + 12.0;
    let gutter_w = 2.0 * num_w + 22.0;
    let mut child = ui.new_child(UiBuilder::new().max_rect(r).layout(egui::Layout::top_down(egui::Align::Min)));
    child.set_clip_rect(r.intersect(ui.clip_rect()));
    egui::ScrollArea::both()
        .id_salt(("changes-diff", agent, commit, &file.path))
        .auto_shrink([false, false])
        .content_margin(egui::Margin::ZERO)
        .show_viewport(&mut child, |ui, viewport| {
            ui.spacing_mut().item_spacing = egui::Vec2::ZERO;
            let width = (gutter_w + 8.0 + widest as f32 * cw + 32.0).max(r.width());
            let (all, _) = ui.allocate_exact_size(vec2(width, rows as f32 * DIFF_ROW + 16.0), Sense::hover());
            let painter = ui.painter();
            let clip = ui.clip_rect();
            let text_left = all.min.x + gutter_w + 8.0;
            let first = ((viewport.min.y - 6.0) / DIFF_ROW).floor().max(0.0) as usize;
            let last = (((viewport.max.y - 6.0) / DIFF_ROW).ceil().max(0.0) as usize + 1).min(rows);
            for i in first..last {
                let y = all.min.y + 6.0 + i as f32 * DIFF_ROW;
                let row = Rect::from_x_y_ranges(clip.min.x..=clip.max.x, y..=y + DIFF_ROW);
                let gutter = Rect::from_x_y_ranges(clip.min.x..=clip.min.x + gutter_w, y..=y + DIFF_ROW);
                let Some(line) = file.lines.get(i) else {
                    let text = "The diff is too long and was cut here.";
                    painter.text(pos2(text_left, row.center().y), Align2::LEFT_CENTER, text, theme::ui(12.5), c(theme::TEXT_2));
                    continue;
                };
                let (tint, strong, sign, text_color) = match line.kind {
                    LineKind::Added => (ca(ADD, 24), ca(ADD, 70), "+", c(theme::TEXT)),
                    LineKind::Removed => (ca(DEL, 24), ca(DEL, 70), "−", c(theme::TEXT)),
                    LineKind::Hunk => (c([0x15, 0x1A, 0x25]), Color32::TRANSPARENT, "", ca(theme::NEEDS_YOU, 190)),
                    LineKind::Note => (Color32::TRANSPARENT, Color32::TRANSPARENT, "", c(theme::TEXT_2)),
                    LineKind::Context => (Color32::TRANSPARENT, Color32::TRANSPARENT, "", c([0xA9, 0xAF, 0xBC])),
                };
                painter.rect_filled(row, CornerRadius::ZERO, tint);
                for range in &line.changed {
                    let col = line.text.get(..range.start).map_or(0, |s| s.chars().count()) as f32;
                    let len = line.text.get(range.clone()).map_or(0, |s| s.chars().count()) as f32;
                    let mark = Rect::from_min_size(pos2(text_left + col * cw, y + 1.0), vec2((len * cw).max(2.0), DIFF_ROW - 2.0));
                    painter.rect_filled(mark, CornerRadius::same(2), strong);
                }
                let text_font = if line.kind == LineKind::Note { theme::ui(12.0) } else { font.clone() };
                painter.text(pos2(text_left, row.center().y), Align2::LEFT_CENTER, &line.text, text_font, text_color);
                // The gutter covers text scrolled under it, in the row's tint.
                painter.rect_filled(gutter, CornerRadius::ZERO, c(theme::PANEL));
                painter.rect_filled(gutter, CornerRadius::ZERO, tint);
                for (n, x) in [(line.old, gutter.min.x + num_w), (line.new, gutter.min.x + 2.0 * num_w)] {
                    if let Some(n) = n {
                        painter.text(pos2(x, row.center().y), Align2::RIGHT_CENTER, n.to_string(), font.clone(), c([0x4A, 0x51, 0x60]));
                    }
                }
                let sign_color = if line.kind == LineKind::Added { c(ADD) } else { c(DEL) };
                painter.text(pos2(gutter.max.x - 8.0, row.center().y), Align2::RIGHT_CENTER, sign, font.clone(), sign_color);
            }
            let x = clip.min.x + gutter_w;
            painter.line_segment([pos2(x, clip.min.y), pos2(x, clip.max.y)], Stroke::new(1.0, c(LINE)));
        });
}

fn start_poll(dir: PathBuf, known: Option<(String, String)>) -> Job<Polled> {
    let (tx, rx) = mpsc::channel();
    spawn("changes-poll", move || {
        let now = heads(&dir);
        if now.is_some() && now == known {
            let _ = tx.send(None);
            return;
        }
        // The heads come from before the history, so a run committed in
        // between is read again with the next poll.
        let history = load_history(&dir);
        let _ = tx.send(Some((history, now)));
    });
    rx
}

fn start_diff(dir: PathBuf, commit: String) -> Job<Result<Vec<FileDiff>, String>> {
    let (tx, rx) = mpsc::channel();
    spawn("changes-diff", move || {
        let _ = tx.send(load_diff(&dir, &commit));
    });
    rx
}

fn start_undo(dir: PathBuf, commit: String) -> Job<Result<String, String>> {
    let (tx, rx) = mpsc::channel();
    spawn("changes-undo", move || {
        let _ = tx.send(undo(&dir, &commit));
    });
    rx
}

fn spawn(name: &str, job: impl FnOnce() + Send + 'static) {
    if let Err(e) = std::thread::Builder::new().name(name.into()).spawn(job) {
        tracing::warn!("cannot start the {name} thread ({e})");
    }
}

#[cfg(test)]
#[allow(clippy::single_range_in_vec_init)]
mod tests {
    use super::*;
    use crate::ui::editor::review::tests::{TempDir, agent_repo, run};

    /// A commit by the daemon, as runs.rs makes it.
    fn daemon_commit(dir: &Path, message: &str) {
        run(dir, &["add", "-A"]);
        run(dir, &["-c", "user.email=agents@meerora.local", "commit", "-q", "-m", message]);
    }

    #[test]
    fn log_records_parse_with_file_stats() {
        let raw = b"\x1eabc1234def\x1fparent1\x1f1791991823\x1fmeerora\x1fagents@meerora.local\x1frun 2026-10-14 19:41: Fix the timeouts\x1f\0\n\
                    5\t1\trun.py\0-\t-\tlogo.png\0\x1ebbb2222aaa\x1f\x1f1791990000\x1fmeerora\x1fagents@meerora.local\x1fmeerora init\x1f\0\n\
                    3\t0\tagent.toml\0";
        let entries = parse_log(raw);
        assert_eq!(entries.len(), 2);
        let e = &entries[0];
        assert_eq!((e.id.as_str(), e.parents.as_slice(), e.time), ("abc1234def", ["parent1".to_string()].as_slice(), 1_791_991_823));
        assert_eq!((e.kind, e.title.as_str()), (Kind::Run, "Fix the timeouts"));
        assert_eq!(
            e.files,
            [
                FileStat { path: "run.py".into(), added: Some(5), removed: Some(1) },
                FileStat { path: "logo.png".into(), added: None, removed: None }
            ]
        );
        assert_eq!(e.totals(), (5, 1));
        assert_eq!((entries[1].kind, entries[1].parents.len()), (Kind::Meerora, 0));
        assert_eq!(classify(REVIEW_EMAIL, "meerora: keep Ember's change to run.py"), (Kind::Kept, "Kept Ember's change to run.py".into()));
        assert_eq!(classify(REVIEW_EMAIL, "Revert \"run 2026-10-14 19:41: Fix it\""), (Kind::Undo, "Undid \"Fix it\"".into()));
        assert_eq!(classify("me@example.org", "tidy up").0, Kind::Other);
        assert_eq!(
            reverted_commit("This reverts commit 4aae6313e1d0108aebd1263716e17996ac65d76f."),
            Some("4aae6313e1d0108aebd1263716e17996ac65d76f".into())
        );
    }

    #[test]
    fn history_of_an_agent_repository() {
        let repo = agent_repo("changes-history");
        let dir = &repo.0;
        std::fs::write(dir.join("run.py"), "import time\n\ndef run():\n    time.sleep(1)\n    return 2\n").unwrap();
        std::fs::create_dir_all(dir.join("memory")).unwrap();
        std::fs::write(dir.join("memory/notes.md"), "one\ntwo\n").unwrap();
        daemon_commit(dir, "run 2026-10-14 19:12: Sleep before returning");
        // You reviewed everything up to here.
        let reviewed = run(dir, &["rev-parse", "HEAD"]);
        run(dir, &["update-ref", REVIEWED_REF, &reviewed]);
        std::fs::write(dir.join("memory/notes.md"), "one\nthree\n").unwrap();
        daemon_commit(dir, "run 2026-10-14 19:41: Update the notes");

        let h = load_history(dir).unwrap();
        let titles: Vec<&str> = h.entries.iter().map(|e| e.title.as_str()).collect();
        assert_eq!(titles, ["Update the notes", "Sleep before returning", "run 1: write run.py"], "newest first");
        let newest = &h.entries[0];
        assert_eq!(newest.files, [FileStat { path: "memory/notes.md".into(), added: Some(1), removed: Some(1) }]);
        assert!(newest.to_review() && newest.on_head && newest.undoable());
        let second = &h.entries[1];
        assert_eq!(second.files.len(), 2);
        assert_eq!(second.totals(), (4, 1));
        assert!(second.reviewed, "behind the review ref");
        assert_eq!(h.entries[2].kind, Kind::Other, "the test repository's first commit has no daemon author");
        assert!(!h.entries[2].undoable(), "the first commit stays");

        // Keep in the editor moves the review, and the run counts as reviewed.
        let r = Repo::open(dir).unwrap();
        r.set_reviewed(Path::new("memory/notes.md"), "one\nthree\n", "meerora: keep Ember's change to memory/notes.md").unwrap();
        let h = load_history(dir).unwrap();
        let kept = h.entries.iter().find(|e| e.kind == Kind::Kept).expect("Keep shows in the timeline");
        assert_eq!(kept.title, "Kept Ember's change to memory/notes.md");
        assert!(!kept.on_head && !kept.undoable());
        let update = h.entries.iter().find(|e| e.title == "Update the notes").unwrap();
        assert!(update.reviewed && !update.to_review());
    }

    #[test]
    fn diffs_parse_and_mark_changed_words() {
        let text = "diff --git a/run.py b/run.py\nindex 1..2 100644\n--- a/run.py\n+++ b/run.py\n@@ -3,3 +3,4 @@ def run():\n \
                    \tx = 1\n-    out = process(s)\n+    out = process(s, retries=3)\n+    log(out)\n \treturn x\n\\ No newline at end of file\n\
                    diff --git a/new file.md b/new file.md\nnew file mode 100644\n--- /dev/null\n+++ b/new file.md\n@@ -0,0 +1 @@\n+hello\n\
                    diff --git a/logo.png b/logo.png\ndeleted file mode 100644\nBinary files a/logo.png and /dev/null differ\n";
        let files = parse_diff(text);
        assert_eq!(files.len(), 3);
        let f = &files[0];
        assert_eq!((f.path.as_str(), f.change, f.binary), ("run.py", FileChange::Modified, false));
        let kinds: Vec<LineKind> = f.lines.iter().map(|l| l.kind).collect();
        use LineKind::*;
        assert_eq!(kinds, [Hunk, Context, Removed, Added, Added, Context, Note]);
        assert_eq!(f.lines[1].text, "    x = 1", "tabs are expanded");
        assert_eq!((f.lines[1].old, f.lines[1].new), (Some(3), Some(3)));
        assert_eq!((f.lines[2].old, f.lines[2].new), (Some(4), None));
        assert_eq!((f.lines[3].old, f.lines[3].new), (None, Some(4)));
        assert_eq!((f.lines[4].new, f.lines[5].old, f.lines[5].new), (Some(5), Some(5), Some(6)));
        assert_eq!(f.lines[6].text, "No newline at end of file");
        // Only the inserted arguments of the changed line get the strong tint.
        let added = &f.lines[3];
        assert_eq!(added.changed, [19..30]);
        assert_eq!(&added.text[19..30], ", retries=3");
        assert!(f.lines[2].changed.is_empty(), "nothing of the old line is gone");
        assert!(f.lines[4].changed.is_empty(), "an added line without a partner");
        assert_eq!((files[1].path.as_str(), files[1].change), ("new file.md", FileChange::Added));
        assert_eq!(files[1].lines.last().map(|l| (l.kind, l.text.as_str(), l.new)), Some((Added, "hello", Some(1))));
        assert_eq!((files[2].path.as_str(), files[2].change, files[2].binary), ("logo.png", FileChange::Deleted, true));
    }

    #[test]
    fn colouring_ranges_skip_rewritten_lines() {
        assert_eq!(changed_ranges("let a = 1;", "let a = 2;"), (vec![8..9], vec![8..9]));
        assert_eq!(changed_ranges("same", "same"), (vec![], vec![]));
        assert_eq!(changed_ranges("abcdef", "uvwxyz"), (vec![], vec![]), "rewritten lines get no ranges");
        let (old, new) = changed_ranges("run_batch(b, cfg)", "run_batch(batch, cfg)");
        assert!(old.is_empty() || old.iter().all(|r| r.end <= 12), "{old:?}");
        assert_eq!(new.iter().map(|r| &"run_batch(batch, cfg)"[r.clone()]).collect::<String>(), "batch", "whole words");
        let (old, new) = changed_ranges("- Jana Novak, prefers calls on Tuesday", "- Jana Novak, prefers calls on Thursday afternoon");
        assert_eq!((old, new), (vec![31..38], vec![31..49]), "Tuesday became Thursday afternoon");
        assert_eq!(whole_words("ab cd", vec![1..2, 4..5]), [0..2, 3..5]);
    }

    #[test]
    fn undo_makes_a_revert_commit_and_joins_the_review() {
        let repo = agent_repo("changes-undo");
        let dir = &repo.0;
        let reviewed = run(dir, &["rev-parse", "HEAD"]);
        run(dir, &["update-ref", REVIEWED_REF, &reviewed]);
        std::fs::write(dir.join("run.py"), "import time\n\ndef run():\n    return 2\n").unwrap();
        std::fs::write(dir.join("notes.md"), "new\n").unwrap();
        daemon_commit(dir, "run 2026-10-14 19:41: Return two");
        // You kept the change to run.py, not the new notes.
        let r = Repo::open(dir).unwrap();
        r.set_reviewed(Path::new("run.py"), "import time\n\ndef run():\n    return 2\n", "meerora: keep Ember's change to run.py").unwrap();
        let target = run(dir, &["rev-parse", "HEAD"]);

        let new = undo(dir, &target).unwrap();
        assert_eq!(run(dir, &["rev-parse", "HEAD"]), new);
        assert_eq!(run(dir, &["log", "-1", "--format=%s"]), "Revert \"run 2026-10-14 19:41: Return two\"");
        assert_eq!(run(dir, &["log", "-1", "--format=%ae"]), REVIEW_EMAIL);
        assert_eq!(std::fs::read_to_string(dir.join("run.py")).unwrap(), "import time\n\ndef run():\n    return 1\n");
        assert!(!dir.join("notes.md").exists());
        assert_eq!(run(dir, &["status", "--porcelain"]), "");
        assert!(r.changed_files().unwrap().is_empty(), "the editor shows no change of the agent's");

        let h = load_history(dir).unwrap();
        let mine = h.entries.iter().find(|e| e.id == new).unwrap();
        assert_eq!((mine.kind, mine.title.as_str()), (Kind::Undo, "Undid \"Return two\""));
        assert!(mine.reviewed && mine.undoable(), "your own undo needs no review and can be undone");
        assert!(!h.entries.iter().any(|e| e.subject.starts_with(UNDO_FOLD)), "the review bookkeeping stays out of the timeline");
        let undone = h.entries.iter().find(|e| e.id == target).unwrap();
        assert_eq!(undone.undone_by.as_deref(), Some(new.as_str()));
        assert!(!undone.undoable() && !undone.to_review());
        let again = undo(dir, &target).unwrap_err();
        assert!(again.contains("Nothing to undo"), "{again}");
    }

    #[test]
    fn a_conflicting_undo_is_aborted() {
        let repo = agent_repo("changes-conflict");
        let dir = &repo.0;
        std::fs::write(dir.join("run.py"), "import time\n\ndef run():\n    return 2\n").unwrap();
        daemon_commit(dir, "run 2026-10-14 19:12: Return two");
        let target = run(dir, &["rev-parse", "HEAD"]);
        std::fs::write(dir.join("run.py"), "import time\n\ndef run():\n    return 3\n").unwrap();
        daemon_commit(dir, "run 2026-10-14 19:41: Return three");
        let head = run(dir, &["rev-parse", "HEAD"]);

        let err = undo(dir, &target).unwrap_err();
        assert!(err.contains("run.py") && err.contains("aborted"), "{err}");
        assert_eq!(run(dir, &["rev-parse", "HEAD"]), head, "no commit");
        assert_eq!(run(dir, &["status", "--porcelain"]), "", "the repository is clean");
        assert!(!dir.join(".git/REVERT_HEAD").exists());
        assert_eq!(std::fs::read_to_string(dir.join("run.py")).unwrap(), "import time\n\ndef run():\n    return 3\n");
    }

    #[test]
    fn the_tile_draws_without_a_window() {
        let repo = agent_repo("changes-draw");
        let dir = &repo.0;
        std::fs::write(dir.join("run.py"), "import time\n\ndef run():\n    return 2  # a long line that needs sideways scrolling\n")
            .unwrap();
        std::fs::write(dir.join("logo.png"), [0u8, 159, 146, 150]).unwrap();
        daemon_commit(dir, "run 2026-10-14 19:41: Return two");
        let mut st = AgentChanges::new(dir.clone());
        st.set_history(load_history(dir), heads(dir));
        let selected = st.selected.clone().expect("the newest run is selected");
        assert_eq!(st.selected_entry().map(|e| e.title.as_str()), Some("Return two"));
        st.diffs.insert(selected.clone(), Arc::new(load_diff(dir, &selected)));
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        let screen = Rect::from_min_size(egui::Pos2::ZERO, vec2(1200.0, 800.0));
        for (width, file, strip) in [(1000.0, 0, 0), (1000.0, 1, 1), (500.0, 1, 2), (300.0, 0, 0)] {
            st.file = file;
            st.confirm = (strip == 1).then(|| selected.clone());
            st.note = (strip == 2).then(|| ("Undo stopped.".to_string(), true));
            let view = View { id: "ember".into(), name: "Ember".into(), running: strip == 2, selected: true };
            let raw = egui::RawInput { screen_rect: Some(screen), ..Default::default() };
            let mut output = ctx.run_ui(raw, |ui| {
                let head = Rect::from_min_size(pos2(10.0, 10.0), vec2(width, 38.0));
                let body = Rect::from_min_max(pos2(11.0, 49.0), pos2(10.0 + width, 790.0));
                let mut out = Out::default();
                st.draw(ui, &view, head, body, &mut out);
                assert_eq!(out.focused, strip == 1, "a pending undo keeps Esc in the tile");
                assert!(out.open.is_none());
            });
            output.textures_delta.clear();
        }
        let files: Vec<&str> = st.selected_entry().unwrap().files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(files, ["logo.png", "run.py"]);
    }

    #[test]
    fn open_in_editor_asks_for_the_version_of_the_change() {
        let repo = agent_repo("changes-open");
        let dir = &repo.0;
        std::fs::write(dir.join("run.py"), "import time\n\ndef run():\n    return 2\n").unwrap();
        daemon_commit(dir, "run 2026-10-14 19:41: Return two");
        let mut st = AgentChanges::new(dir.clone());
        st.set_history(load_history(dir), heads(dir));
        let selected = st.selected.clone().expect("the newest run is selected");
        st.diffs.insert(selected.clone(), Arc::new(load_diff(dir, &selected)));
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        let view = View { id: "ember".into(), name: "Ember".into(), running: false, selected: true };
        let frame = |st: &mut AgentChanges, events: Vec<egui::Event>| {
            let screen = Rect::from_min_size(egui::Pos2::ZERO, vec2(1200.0, 800.0));
            let raw = egui::RawInput { screen_rect: Some(screen), events, ..Default::default() };
            let mut out = Out::default();
            let mut output = ctx.run_ui(raw, |ui| {
                let head = Rect::from_min_size(pos2(10.0, 10.0), vec2(1000.0, 38.0));
                let body = Rect::from_min_max(pos2(11.0, 49.0), pos2(1010.0, 790.0));
                st.draw(ui, &view, head, body, &mut out);
            });
            output.textures_delta.clear();
            (out, output.shapes)
        };
        let (_, shapes) = frame(&mut st, Vec::new());
        let at = shapes
            .iter()
            .find_map(|s| match &s.shape {
                egui::Shape::Text(t) if t.galley.text() == "Open in editor" => Some(t.visual_bounding_rect().center()),
                _ => None,
            })
            .expect("an Open in editor button");
        let press =
            |pressed| egui::Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed, modifiers: Default::default() };
        frame(&mut st, vec![egui::Event::PointerMoved(at), press(true)]);
        let (out, _) = frame(&mut st, vec![press(false)]);
        assert_eq!(out.open, Some(("run.py".to_string(), selected)), "the file at the commit of the change, not as it is now");
    }

    #[test]
    fn no_history_outside_own_repositories() {
        let tmp = TempDir::new("changes-off");
        run(&tmp.0, &["init", "-q"]);
        let nested = tmp.0.join("agents/wren");
        std::fs::create_dir_all(&nested).unwrap();
        let reason = load_history(&nested).unwrap_err();
        let (title, text) = no_history("Wren", &reason);
        assert_eq!(title, "No history of its own");
        assert!(text.contains("inside the git work tree") && text.contains("meerora-agentd init"), "{text}");
    }

    #[test]
    fn readable_undo_errors() {
        assert_eq!(and_list(&["a".into(), "b".into(), "c".into(), "d".into()]), "a, b and 2 more files");
        let e = undo_error("", "error: Your local changes to the following files would be overwritten by merge:\n\trun.py\n", &[]);
        assert_eq!(e, "Undo stopped, because run.py changed since the last commit. Nothing was changed.");
        assert!(undo_error("", "fatal: Unable to create '/x/.git/index.lock': File exists.", &[]).contains("busy"));
        assert_eq!(undo_error("", "fatal: bad revision 'zzz'", &[]), "Undo failed (bad revision 'zzz').");
        assert_eq!(duration(45_000), "45 s");
        assert_eq!(duration(192_000), "3 min 12 s");
        assert_eq!(duration(7_500_000), "2 h 05 min");
    }
}
