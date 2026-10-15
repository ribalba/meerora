//! The Git app of the agent view (D11): review what an agent changed in a
//! repository, edit right in the diff, stage, commit and push.
//!
//! Each instance (`git:1`, `git:2`, D30) shows one repository: one of the
//! agent's workspace folders, its own directory, a recent repository or a
//! path you type. The head shows the repository, its branch with ahead and
//! behind, and "Changed by <agent> 2 min ago" when the agent's run patched
//! files in it (D12). The left column lists the staged, changed and
//! untracked files, each staged or unstaged with a click, and the commit box
//! below them. The right side shows the selected file's diff side by side or
//! unified, with syntax colours, word-level highlights and per-hunk stage,
//! unstage and discard. The working side is editable in place: click a line
//! and type, Enter splits it, Backspace at its start joins it with the line
//! above, Ctrl+S saves. Unsaved edits block staging, committing and
//! switching files.
//!
//! git runs on threads, never on the UI thread. The status is read every two
//! seconds while the tile shows and at once when it gets focus. Which git
//! runs where is described in git/repo.rs.

mod diff;
pub mod repo;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, SystemTime};

use egui::text::{CCursor, CCursorRange, LayoutJob, TextFormat};
use egui::{
    Align2, Color32, CornerRadius, FontId, Id, Key, KeyboardShortcut, Modifiers, Rect, Sense, Stroke, StrokeKind, Ui, UiBuilder, pos2, vec2,
};
use serde::{Deserialize, Serialize};

use repo::{Areas, FileStatus, Git, Kind, Mode, Remote, Status, Work};

use super::changes::{ADD, DEL, changed_ranges};
use crate::engine::Core;
use crate::theme::{self, c, ca};
use crate::ui::editor::highlight::{self, Lang};
use crate::ui::{AppTab, ButtonStyle, button, fit, hline, pill, vline};
use crate::world::ActionEntry;
use diff::{Doc, Tag, content};

/// Seconds between status reads while the tile shows.
const POLL_EVERY: f64 = 2.0;
/// Recent repositories kept in `recent-repos.json`.
const MAX_RECENT: usize = 12;
/// Files listed per section at most.
const MAX_FILES: usize = 1500;

const ROW_H: f32 = 19.0;
const HUNK_H: f32 = 30.0;
const FONT: f32 = 12.0;
const FILE_ROW: f32 = 26.0;
const SECTION_H: f32 = 30.0;
const COMMIT_H: f32 = 206.0;
const DIFF_HEAD: f32 = 42.0;
const STRIP_H: f32 = 50.0;
const LOG_H: f32 = 140.0;

const DIM: [u8; 3] = [0x8A, 0x92, 0xA3];
const NUMBER: [u8; 3] = [0x4A, 0x51, 0x60];
const BAR_BG: [u8; 3] = [0x0F, 0x12, 0x18];
const FILLER: [u8; 3] = [0x0D, 0x10, 0x15];
const FIELD: [u8; 3] = [0x16, 0x1A, 0x23];
const FIELD_LINE: [u8; 3] = [0x2A, 0x30, 0x3D];
const SELECTED: [u8; 3] = [0x1A, 0x1F, 0x2A];
const HOVERED: [u8; 3] = [0x16, 0x1A, 0x23];
const WARN: [u8; 3] = [0xF2, 0xB4, 0x5A];

type Job<T> = Receiver<T>;

/// Which list a file was picked from, which decides its diff.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Section {
    /// HEAD against the index, read-only.
    Staged,
    /// The index against the work tree, editable.
    Changes,
    /// Nothing against the work tree, editable.
    Untracked,
}

impl Section {
    fn title(self) -> &'static str {
        match self {
            Section::Staged => "STAGED",
            Section::Changes => "CHANGES",
            Section::Untracked => "UNTRACKED",
        }
    }

    fn holds(self, f: &FileStatus) -> bool {
        match self {
            Section::Staged => f.staged(),
            Section::Changes => f.changed(),
            Section::Untracked => f.untracked(),
        }
    }
}

// Recent repositories and what each instance showed.

#[derive(Default, Serialize, Deserialize)]
struct Saved {
    #[serde(default)]
    recent: Vec<PathBuf>,
    /// The repository of each instance, by agent and instance number.
    #[serde(default)]
    open: BTreeMap<String, BTreeMap<u32, PathBuf>>,
}

/// `<data dir>/recent-repos.json`.
struct Store {
    path: PathBuf,
    saved: Saved,
}

impl Store {
    fn load(data_dir: &Path) -> Store {
        let path = data_dir.join("recent-repos.json");
        let saved = std::fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
        Store { path, saved }
    }

    fn remember(&mut self, agent: &str, n: u32, top: &Path) {
        self.saved.recent.retain(|p| p != top);
        self.saved.recent.insert(0, top.to_path_buf());
        self.saved.recent.truncate(MAX_RECENT);
        self.saved.open.entry(agent.to_string()).or_default().insert(n, top.to_path_buf());
        let text = serde_json::to_string_pretty(&self.saved).unwrap_or_default();
        let tmp = self.path.with_extension("json.tmp");
        if let Err(e) = std::fs::write(&tmp, text).and_then(|()| std::fs::rename(&tmp, &self.path)) {
            tracing::warn!("cannot save {} ({e})", self.path.display());
        }
    }
}

/// Every Git app instance, by agent and instance number.
#[derive(Default)]
pub struct Gits {
    instances: HashMap<(String, u32), Instance>,
    store: Option<Store>,
}

// One file's diff.

#[derive(Clone, Debug, PartialEq)]
struct FileKey {
    section: Section,
    path: String,
    head: Option<String>,
    index: Option<String>,
    /// Modification time and size of the work tree file, for the editable sections.
    stamp: Option<(SystemTime, u64)>,
}

enum Content {
    Text(Doc),
    /// Not shown as a diff: a title and why.
    Note(&'static str, String),
}

/// Word-level highlights of line pairs, by base and working line.
type Words = HashMap<(usize, usize), (Vec<Range<usize>>, Vec<Range<usize>>)>;

/// Coloured byte ranges of one line.
type Tokens = Vec<(Range<usize>, highlight::Kind)>;

struct FileView {
    key: FileKey,
    content: Content,
    lang: Lang,
    base_tokens: Vec<Tokens>,
    work_tokens: Vec<Tokens>,
    /// Widest line in characters, for sideways scrolling.
    widest: usize,
    /// The working side can be edited: a text file in the work tree.
    editable: bool,
    /// The file changed on disk while you had unsaved edits.
    disk_changed: bool,
}

impl FileView {
    fn doc(&self) -> Option<&Doc> {
        match &self.content {
            Content::Text(d) => Some(d),
            Content::Note(..) => None,
        }
    }

    fn dirty(&self) -> bool {
        self.doc().is_some_and(Doc::dirty)
    }

    /// Colours and width again after an edit.
    fn retokenize(&mut self) {
        if let Content::Text(doc) = &self.content {
            self.work_tokens = tokens_by_line(&doc.work_text(), self.lang);
            self.widest = widest(doc);
        }
    }
}

/// Syntax colours of `text` line by line.
fn tokens_by_line(text: &str, lang: Lang) -> Vec<Tokens> {
    let tokens = highlight::tokens(text, lang);
    let mut out = Vec::new();
    let mut at = 0;
    let mut t = 0;
    for line in text.split_inclusive('\n') {
        let (start, end) = (at, at + line.len());
        let mut mine = Vec::new();
        while t < tokens.len() && tokens[t].0.end <= start {
            t += 1;
        }
        let mut k = t;
        while k < tokens.len() && tokens[k].0.start < end {
            let (r, kind) = &tokens[k];
            mine.push((r.start.max(start) - start..r.end.min(end) - start, *kind));
            k += 1;
        }
        out.push(mine);
        at = end;
    }
    out
}

fn widest(doc: &Doc) -> usize {
    doc.base.iter().chain(&doc.work).map(|l| display_width(content(l))).max().unwrap_or(0)
}

/// Columns of `text` with tabs as four spaces.
fn display_width(text: &str) -> usize {
    text.chars().map(|c| if c == '\t' { 4 } else { 1 }).sum()
}

/// `text` with tabs as four spaces.
fn display(text: &str) -> String {
    text.replace('\t', "    ")
}

/// The character index of display column `col` in `text`.
fn char_at_column(text: &str, col: usize) -> usize {
    let mut x = 0;
    for (i, ch) in text.chars().enumerate() {
        let w = if ch == '\t' { 4 } else { 1 };
        if x + w / 2 >= col && x <= col {
            return i;
        }
        x += w;
        if x > col {
            return i + 1;
        }
    }
    text.chars().count()
}

/// What a load thread brings for a file.
struct Loaded {
    key: FileKey,
    view: Result<FileView, String>,
}

/// Reads both sides of a file and aligns them, on a thread.
fn load_file(git: &Git, key: FileKey, entry: Option<FileStatus>) -> Loaded {
    let view = (|| {
        let lang = Lang::of(Path::new(&key.path));
        let blob = |oid: &Option<String>| -> Result<Vec<u8>, String> { oid.as_deref().map_or(Ok(Vec::new()), |o| git.blob(o)) };
        let entry = entry.unwrap_or(FileStatus { path: key.path.clone(), x: '.', y: '.', kind: Kind::Untracked, head: None, index: None });
        let (base, work, on_disk) = match key.section {
            Section::Staged => (blob(&entry.head)?, if entry.x == 'D' { Vec::new() } else { blob(&entry.index)? }, false),
            Section::Changes => {
                let base = if entry.kind == Kind::Conflict { blob(&entry.head)? } else { blob(&entry.index)? };
                let disk = git.disk(&key.path)?;
                let exists = disk.is_some();
                (base, disk.unwrap_or_default(), exists)
            }
            Section::Untracked => {
                let disk = git.disk(&key.path)?;
                let exists = disk.is_some();
                (Vec::new(), disk.unwrap_or_default(), exists)
            }
        };
        let note = |title: &'static str, text: &str| FileView {
            key: key.clone(),
            content: Content::Note(title, text.to_string()),
            lang,
            base_tokens: Vec::new(),
            work_tokens: Vec::new(),
            widest: 0,
            editable: false,
            disk_changed: false,
        };
        let binary = |b: &[u8]| b[..b.len().min(8000)].contains(&0);
        if binary(&base) || binary(&work) {
            return Ok(note("Binary file", "Its changes are not shown. Stage, unstage or discard it as a whole."));
        }
        let (Ok(base), Ok(work)) = (String::from_utf8(base), String::from_utf8(work)) else {
            return Ok(note("Not UTF-8 text", "Its changes are not shown. Stage, unstage or discard it as a whole."));
        };
        let doc = Doc::new(&base, &work);
        let mut view = FileView {
            key: key.clone(),
            base_tokens: tokens_by_line(&base, lang),
            work_tokens: tokens_by_line(&work, lang),
            widest: widest(&doc),
            content: Content::Text(doc),
            lang,
            editable: on_disk
                && key.section != Section::Staged
                && git.mode != Mode::Untrusted
                && work.len() as u64 <= crate::ui::editor::files::MAX_EDIT_BYTES,
            disk_changed: false,
        };
        if view.doc().is_some_and(|d| d.hunks.is_empty()) && base == work {
            view.content = Content::Note("No text changes", "Only the file mode or the line endings changed.".into());
        }
        Ok(view)
    })();
    Loaded { key, view }
}

// The instance.

/// The line you edit in the diff.
struct Edit {
    /// Working line index.
    line: usize,
    text: String,
    /// Put the cursor at this character with the next frame.
    cursor: Option<usize>,
}

/// A question in the strip above the diff.
#[derive(Clone, Debug, PartialEq)]
enum Confirm {
    DiscardHunk(usize),
    DiscardFile(String),
    /// Hooks inside the work tree changed and would run on commit.
    CommitHooks,
    /// The branch has no upstream yet: push it there and track it.
    Upstream {
        remote: String,
        branch: String,
    },
    /// Unsaved edits block what you tried.
    Unsaved,
    /// Trust an untrusted repository, so your own git runs there.
    Trust,
}

/// What a finished background action brings.
enum Done {
    Note(String),
    Committed { id: String, subject: String },
}

#[derive(Default)]
struct Instance {
    git: Option<Git>,
    opening: Option<(PathBuf, Job<Result<Opened, String>>)>,
    open_error: Option<String>,
    picker: bool,
    path_input: String,
    status: Option<Result<Status, String>>,
    poll: Option<Job<Result<Status, String>>>,
    polled_at: f64,
    refresh: bool,
    hooks: Option<String>,
    remotes: Vec<String>,
    /// A warning about the repository that stays, such as a sandbox that is off.
    warning: Option<String>,
    selected: Option<(Section, String)>,
    file: Option<FileView>,
    load: Option<(FileKey, Job<Loaded>)>,
    unified: bool,
    hscroll: f32,
    edit: Option<Edit>,
    confirm: Option<Confirm>,
    action: Option<(String, Job<Result<Done, String>>)>,
    note: Option<(String, bool)>,
    subject: String,
    body: String,
    amend: bool,
    amend_job: Option<Job<Result<String, String>>>,
    remote: Option<Remote>,
    log_open: bool,
    was_selected: bool,
    had_focus: bool,
    /// A snapshot asked to show the first staged file.
    prefer_staged: bool,
    /// The data directory, for `workspace-trust.json`.
    data_dir: PathBuf,
    /// The configuration and attribute files of an untrusted repository, shown read-only.
    show_config: Option<String>,
    /// An agent that can write this untrusted repository runs now, so no git runs there.
    hold: Option<String>,
    /// Open the repository again with the next frame, after you trusted it.
    reopen: bool,
    /// Word-level highlights of line pairs, by base and working line.
    words: Words,
}

/// An opened repository with what the app asks once.
struct Opened {
    git: Git,
    hooks: Option<String>,
    remotes: Vec<String>,
    warning: Option<String>,
}

/// What the tile needs from the agent view and gives back to it.
struct View<'a> {
    agent: String,
    n: u32,
    name: String,
    color: [u8; 3],
    selected: bool,
    agent_dir: Option<PathBuf>,
    /// Workspace folders as written, each with the checked path or why it is refused.
    workspace: Vec<(String, Result<PathBuf, String>)>,
    recent: &'a [PathBuf],
    /// Seconds since the agent last patched a file in this repository.
    changed_ago: Option<f64>,
    /// Files of this repository the agent patched, relative to its top.
    touched: HashSet<String>,
    /// The name of the agent that could write an untrusted repository's `.git` when it appeared.
    made_by: Option<String>,
}

#[derive(Default)]
struct Out {
    clicked: bool,
    focused: bool,
    /// Open this folder as the repository.
    open: Option<PathBuf>,
}

/// Draws Git instance `n` of agent `index`. Returns true while it has the keyboard.
pub fn tile(ui: &mut Ui, core: &mut Core, index: usize, n: u32, head: Rect, body: Rect) -> bool {
    let a = &core.world.agents[index];
    let (agent, name, color) = (a.id.clone(), a.name.clone(), a.color);
    let selected = core.ui.tab == AppTab::Git(n);
    let now = core.world.time;
    let data_dir = core.assets.data_dir.clone();
    let store = core.ui.git.store.get_or_insert_with(|| Store::load(&data_dir));
    let remembered = store.saved.open.get(&agent).and_then(|m| m.get(&n)).cloned();
    let recent = store.saved.recent.clone();
    let mut inst = core.ui.git.instances.remove(&(agent.clone(), n)).unwrap_or_default();
    let agent_dir = core.agent_dir(&agent).map(|d| d.path.clone());
    let home = user_home();
    let workspace: Vec<(String, Result<PathBuf, String>)> = core
        .agent_dir(&agent)
        .map(|d| {
            d.config
                .workspace
                .iter()
                .map(|w| (w.display().to_string(), meerora_protocol::workspace::check(w, home.as_deref(), &data_dir)))
                .collect()
        })
        .unwrap_or_default();

    // The first time: the repository it showed last, else the first
    // workspace folder or the agent's directory that no other instance shows.
    if inst.git.is_none() && inst.opening.is_none() && !inst.picker && inst.open_error.is_none() {
        let taken: Vec<PathBuf> =
            core.ui.git.instances.iter().filter(|(k, _)| k.0 == agent).filter_map(|(_, i)| i.git.as_ref().map(|g| g.top.clone())).collect();
        let candidates = workspace.iter().filter_map(|(_, r)| r.as_ref().ok().cloned()).chain(agent_dir.clone());
        let pick = remembered.or_else(|| candidates.filter(|p| p.join(".git").exists()).find(|p| !taken.iter().any(|t| t == p)));
        match pick {
            Some(path) => inst.start_open(path, areas(core), data_dir.clone()),
            None => inst.picker = true,
        }
    }

    // An untrusted repository gets no git while an agent that can write it runs.
    inst.hold = inst.git.as_ref().filter(|g| g.mode == Mode::Untrusted).and_then(|g| writer_running(core, &g.top));
    inst.update(now);
    let made_by = inst.git.as_ref().and_then(|g| g.made_by.clone()).map(|id| core.world.agent(&id).map_or(id, |a| a.name.clone()));
    let (changed_ago, touched) = match &inst.git {
        Some(git) => agent_changes(&core.world.agents[index].actions, git, agent_dir.as_deref(), now),
        None => (None, HashSet::new()),
    };
    let view =
        View { agent: agent.clone(), n, name, color, selected, agent_dir, workspace, recent: &recent, changed_ago, touched, made_by };
    let mut out = Out::default();
    inst.draw(ui, &view, head, body, now, &mut out);
    if std::mem::take(&mut inst.reopen) && out.open.is_none() {
        out.open = inst.git.as_ref().map(|g| g.top.clone());
    }
    if let Some(path) = out.open {
        inst.start_open(path, areas(core), data_dir);
    }
    if out.clicked {
        core.ui.tab = AppTab::Git(n);
    }
    if let Some(git) = inst.git.as_ref().filter(|g| inst.opening.is_none() && remembered_differs(core, &agent, n, &g.top)) {
        let top = git.top.clone();
        if let Some(store) = core.ui.git.store.as_mut() {
            store.remember(&agent, n, &top);
        }
    }
    core.ui.git.instances.insert((agent, n), inst);
    out.focused
}

fn remembered_differs(core: &Core, agent: &str, n: u32, top: &Path) -> bool {
    core.ui.git.store.as_ref().is_some_and(|s| s.saved.open.get(agent).and_then(|m| m.get(&n)).map(PathBuf::as_path) != Some(top))
}

fn user_home() -> Option<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME")?);
    (home.is_absolute() && home != Path::new("/")).then_some(home)
}

fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// Where agents can write: every agent's directory and workspace folders.
fn areas(core: &Core) -> Areas {
    let home = user_home();
    let mut areas = Areas::default();
    for a in &core.world.agents {
        if let Some(dir) = core.agent_dir(&a.id) {
            areas.agents.push(canonical(&dir.path));
            let (ok, _) = meerora_protocol::workspace::resolve(&dir.config.workspace, home.as_deref(), &core.assets.data_dir);
            areas.workspaces.extend(ok);
        }
    }
    areas
}

/// The name of a running agent that can write `top`: in its directory or
/// one of its workspace folders.
fn writer_running(core: &Core, top: &Path) -> Option<String> {
    let home = user_home();
    core.world.agents.iter().filter(|a| a.run.is_some()).find_map(|a| {
        let dir = core.agent_dir(&a.id)?;
        let (folders, _) = meerora_protocol::workspace::resolve(&dir.config.workspace, home.as_deref(), &core.assets.data_dir);
        let writes = top.starts_with(canonical(&dir.path)) || folders.iter().any(|f| top.starts_with(f) || f.starts_with(top));
        writes.then(|| a.name.clone())
    })
}

/// When the agent last patched a file in the repository, in seconds, and
/// which files it patched (D12). `editor.patch` names files relative to the
/// agent's directory, or absolute outside it.
fn agent_changes(actions: &[ActionEntry], git: &Git, agent_dir: Option<&Path>, now: f64) -> (Option<f64>, HashSet<String>) {
    let mut ago = None;
    let mut touched = HashSet::new();
    for action in actions.iter().rev().filter(|a| a.tool == "editor.patch") {
        // The full target, since a long path is cut in the summary.
        for part in action.target().split(", ") {
            let path = Path::new(part.trim().trim_end_matches('…'));
            let full = if path.is_absolute() {
                path.to_path_buf()
            } else {
                match agent_dir {
                    Some(dir) => canonical(dir).join(path),
                    None => continue,
                }
            };
            if let Ok(rel) = full.strip_prefix(&git.top) {
                ago.get_or_insert(now - action.at);
                touched.insert(rel.display().to_string());
            }
        }
    }
    (ago, touched)
}

/// "just now", "12 s ago", "2 min ago" or "3 h ago".
fn ago(seconds: f64) -> String {
    match seconds.max(0.0) as u64 {
        0..5 => "just now".into(),
        s @ 5..60 => format!("{s} s ago"),
        s @ 60..3600 => format!("{} min ago", s / 60),
        s => format!("{} h ago", s / 3600),
    }
}

fn spawn<T: Send + 'static>(name: &str, job: impl FnOnce() -> T + Send + 'static) -> Job<T> {
    let (tx, rx) = mpsc::channel();
    if let Err(e) = std::thread::Builder::new().name(name.into()).spawn(move || {
        let _ = tx.send(job());
    }) {
        tracing::warn!("cannot start the {name} thread ({e})");
    }
    rx
}

fn take<T>(job: &mut Option<Job<T>>) -> Option<Option<T>> {
    let rx = job.as_ref()?;
    match rx.try_recv() {
        Ok(v) => {
            *job = None;
            Some(Some(v))
        }
        Err(TryRecvError::Disconnected) => {
            *job = None;
            Some(None)
        }
        Err(TryRecvError::Empty) => None,
    }
}

impl Instance {
    fn start_open(&mut self, path: PathBuf, areas: Areas, data_dir: PathBuf) {
        if self.dirty() {
            self.confirm = Some(Confirm::Unsaved);
            return;
        }
        self.open_error = None;
        self.data_dir = data_dir.clone();
        let target = path.clone();
        let job = spawn("git-open", move || {
            let trust = meerora_protocol::workspace::Trust::load(&data_dir);
            let git = repo::open(&target, &areas, &trust, &[])?;
            let hooks = git.hooks_in_tree();
            let remotes = git.remotes();
            let sandbox_off = meerora_protocol::Settings::load(&data_dir).is_ok_and(|s| !s.sandbox.enabled);
            let in_workspace = areas.workspaces.iter().any(|w| git.top.starts_with(w));
            let warning = (sandbox_off && in_workspace && git.mode == Mode::User).then(|| {
                "The sandbox is off, so agents could have changed this repository's git settings. Your git runs here anyway.".to_string()
            });
            Ok(Opened { git, hooks, remotes, warning })
        });
        self.opening = Some((path, job));
    }

    /// Takes over an opened repository.
    fn opened(&mut self, o: Opened) {
        let same = self.git.as_ref().is_some_and(|g| g.top == o.git.top && g.mode == o.git.mode);
        self.git = Some(o.git);
        self.hooks = o.hooks;
        self.remotes = o.remotes;
        self.warning = o.warning;
        self.picker = false;
        self.open_error = None;
        if !same {
            self.status = None;
            self.selected = None;
            self.file = None;
            self.load = None;
            self.edit = None;
            self.confirm = None;
            self.note = None;
            self.remote = None;
            self.words.clear();
        }
        self.show_config = None;
        self.refresh = true;
    }

    fn dirty(&self) -> bool {
        self.file.as_ref().is_some_and(FileView::dirty)
    }

    fn busy(&self) -> bool {
        self.action.is_some()
    }

    fn status_ok(&self) -> Option<&Status> {
        self.status.as_ref().and_then(|s| s.as_ref().ok())
    }

    /// Takes finished work and starts new work.
    fn update(&mut self, now: f64) {
        if let Some((_, rx)) = &self.opening {
            match rx.try_recv() {
                Ok(Ok(opened)) => {
                    self.opening = None;
                    self.opened(opened);
                }
                Ok(Err(e)) => {
                    self.opening = None;
                    self.open_error = Some(e);
                    self.picker = true;
                }
                Err(TryRecvError::Disconnected) => self.opening = None,
                Err(TryRecvError::Empty) => {}
            }
        }
        if let Some(Some(status)) = take(&mut self.poll) {
            self.status = Some(status);
            self.reconcile();
        }
        if let Some((key, rx)) = &self.load {
            match rx.try_recv() {
                Ok(loaded) => {
                    let wanted = *key == loaded.key;
                    self.load = None;
                    if wanted {
                        self.loaded(loaded);
                    }
                }
                Err(TryRecvError::Disconnected) => self.load = None,
                Err(TryRecvError::Empty) => {}
            }
        }
        if let Some((_, rx)) = &self.action {
            match rx.try_recv() {
                Ok(result) => {
                    self.action = None;
                    match result {
                        Ok(Done::Note(text)) => self.note = Some((text, false)),
                        Ok(Done::Committed { id, subject }) => {
                            self.note = Some((format!("Committed {id}: {subject}"), false));
                            self.subject.clear();
                            self.body.clear();
                            self.amend = false;
                        }
                        Err(e) => self.note = Some((e, true)),
                    }
                    self.refresh = true;
                }
                Err(TryRecvError::Disconnected) => self.action = None,
                Err(TryRecvError::Empty) => {}
            }
        }
        let mut offer = false;
        if let Some(remote) = &mut self.remote
            && remote.result.is_none()
            && !remote.running()
        {
            self.refresh = true;
            self.note = match remote.result.as_ref() {
                Some(Ok(())) if remote.work == Work::Push => Some(("Pushed.".into(), false)),
                Some(Ok(())) => Some(("Fetched.".into(), false)),
                Some(Err(e)) => {
                    offer = e.contains("no upstream");
                    Some((e.clone(), true))
                }
                None => None,
            };
        }
        if offer {
            self.push_upstream_offer();
        }
        if let Some(Some(result)) = take(&mut self.amend_job) {
            match result {
                Ok(message) if self.subject.trim().is_empty() => {
                    let (subject, body) = message.split_once("\n\n").unwrap_or((message.as_str(), ""));
                    self.subject = subject.trim().to_string();
                    self.body = body.trim().to_string();
                }
                Ok(_) => {}
                Err(e) => self.note = Some((e, true)),
            }
        }
        // The status, every two seconds or at once after a change.
        if let Some(git) = &self.git
            && self.poll.is_none()
            && self.action.is_none()
            && self.opening.is_none()
            && self.hold.is_none()
            && (self.refresh || now - self.polled_at >= POLL_EVERY)
        {
            self.refresh = false;
            self.polled_at = now;
            let git = git.clone();
            self.poll = Some(spawn("git-status", move || git.status()));
        }
        self.schedule_load();
    }

    /// Keeps the selection on a file that still has changes, or picks one.
    fn reconcile(&mut self) {
        let Some(status) = self.status_ok() else { return };
        let holds = |s: Section, p: &str| status.file(p).is_some_and(|f| s.holds(f));
        let pick = match &self.selected {
            Some((s, p)) if holds(*s, p) => None,
            Some((_, p)) if self.dirty() => Some(self.selected.clone().map(|(s, _)| (s, p.clone()))),
            Some((_, p)) => {
                let moved =
                    [Section::Changes, Section::Untracked, Section::Staged].into_iter().find(|s| holds(*s, p)).map(|s| (s, p.clone()));
                Some(moved.or_else(|| first(status)))
            }
            None => Some(first(status)),
        };
        if let Some(pick) = pick {
            if pick != self.selected {
                self.edit = None;
                self.confirm = self.confirm.take().filter(|c| matches!(c, Confirm::Upstream { .. } | Confirm::CommitHooks));
            }
            self.selected = pick;
        }
    }

    /// The key of the selected file as it is now.
    fn wanted_key(&self) -> Option<(FileKey, Option<FileStatus>)> {
        let (section, path) = self.selected.clone()?;
        let git = self.git.as_ref()?;
        let entry = self.status_ok().and_then(|s| s.file(&path)).cloned();
        let stamp = match section {
            Section::Staged => None,
            _ => crate::ui::editor::files::stamp(&git.top.join(&path)),
        };
        let (head, index) = entry.as_ref().map_or((None, None), |e| (e.head.clone(), e.index.clone()));
        Some((FileKey { section, path, head, index, stamp }, entry))
    }

    fn schedule_load(&mut self) {
        let Some((key, entry)) = self.wanted_key() else {
            if self.selected.is_none() {
                self.file = None;
            }
            return;
        };
        if let Some(file) = &mut self.file
            && file.key.section == key.section
            && file.key.path == key.path
            && file.dirty()
        {
            // Your edits stay. A change on disk only shows a note.
            file.disk_changed = file.key.stamp != key.stamp;
            return;
        }
        if self.file.as_ref().is_some_and(|f| f.key == key) || self.load.as_ref().is_some_and(|(k, _)| *k == key) {
            return;
        }
        let Some(git) = self.git.clone() else { return };
        let k = key.clone();
        self.load = Some((key, spawn("git-diff", move || load_file(&git, k, entry))));
    }

    fn loaded(&mut self, loaded: Loaded) {
        match loaded.view {
            Ok(view) => {
                let same = self.file.as_ref().is_some_and(|f| f.key.section == view.key.section && f.key.path == view.key.path);
                if !same {
                    self.edit = None;
                    self.hscroll = 0.0;
                }
                self.words.clear();
                self.file = Some(view);
                if let (Some(edit), Some(FileView { content: Content::Text(doc), .. })) = (&self.edit, &mut self.file) {
                    doc.keep(Some(edit.line));
                }
            }
            Err(e) => {
                self.file = Some(FileView {
                    key: loaded.key,
                    content: Content::Note("The diff could not be read", e),
                    lang: Lang::Plain,
                    base_tokens: Vec::new(),
                    work_tokens: Vec::new(),
                    widest: 0,
                    editable: false,
                    disk_changed: false,
                });
            }
        }
    }

    /// Runs `job` with the repository on a thread, then reads the status again.
    fn act(&mut self, label: &str, job: impl FnOnce(&Git) -> Result<Done, String> + Send + 'static) {
        let Some(git) = self.git.clone() else { return };
        if self.busy() {
            return;
        }
        if self.read_only() {
            self.note = Some((repo::READ_ONLY.into(), true));
            return;
        }
        self.note = None;
        self.poll = None;
        self.action = Some((label.to_string(), spawn("git-action", move || job(&git))));
    }

    /// Asks to save or drop the edits first. True when there are none.
    fn clean_or_ask(&mut self) -> bool {
        if self.dirty() {
            self.confirm = Some(Confirm::Unsaved);
            false
        } else {
            true
        }
    }

    fn select(&mut self, section: Section, path: String) {
        if self.selected.as_ref() == Some(&(section, path.clone())) {
            return;
        }
        if !self.clean_or_ask() {
            return;
        }
        self.selected = Some((section, path));
        self.edit = None;
        self.confirm = None;
        self.file = None;
    }

    fn stage_file(&mut self, path: String) {
        if self.clean_or_ask() {
            self.act("Staging", move |g| g.stage(&path).map(|()| Done::Note(format!("Staged {path}"))));
        }
    }

    fn unstage_file(&mut self, path: String) {
        let has_head = self.status_ok().is_some_and(Status::has_head);
        if self.clean_or_ask() {
            self.act("Unstaging", move |g| g.unstage(&path, has_head).map(|()| Done::Note(format!("Unstaged {path}"))));
        }
    }

    fn stage_all(&mut self) {
        if self.clean_or_ask() {
            self.act("Staging", |g| g.stage_all().map(|()| Done::Note("Staged every change".into())));
        }
    }

    fn unstage_all(&mut self) {
        let has_head = self.status_ok().is_some_and(Status::has_head);
        if self.clean_or_ask() {
            self.act("Unstaging", move |g| g.unstage_all(has_head).map(|()| Done::Note("Unstaged everything".into())));
        }
    }

    /// Stages, unstages or discards hunk `h` of the shown file.
    fn hunk(&mut self, h: usize, what: HunkAction) {
        if !self.clean_or_ask() {
            return;
        }
        let Some(file) = &self.file else { return };
        let Some(patch) = file.doc().and_then(|d| d.patch(h, &file.key.path)) else { return };
        let (cached, reverse, done) = match what {
            HunkAction::Stage => (true, false, "Staged the change"),
            HunkAction::Unstage => (true, true, "Unstaged the change"),
            HunkAction::Discard => (false, true, "Discarded the change"),
        };
        self.act("Applying", move |g| {
            g.apply(&patch, cached, reverse)
                .map(|()| Done::Note(done.into()))
                .map_err(|e| format!("{e}. If the file uses line ending or filter conversions, stage or discard it as a whole."))
        });
    }

    /// Writes the buffer to disk. In the agent's directory your edit also
    /// joins the reviewed state, as the editor does it (Q2).
    fn save(&mut self) {
        let Some(git) = self.git.clone() else { return };
        let Some(file) = &mut self.file else { return };
        let Content::Text(doc) = &mut file.content else { return };
        if !doc.dirty() {
            return;
        }
        let path = git.top.join(&file.key.path);
        let (before, text) = (doc.saved().to_string(), doc.work_text());
        if let Err(e) = std::fs::write(&path, &text) {
            self.note = Some((format!("Cannot save {} ({e}).", file.key.path), true));
            return;
        }
        doc.mark_saved();
        file.key.stamp = crate::ui::editor::files::stamp(&path);
        file.disk_changed = false;
        self.note = Some((format!("Saved {}", file.key.path), false));
        self.refresh = true;
        if git.mode == Mode::Agent {
            let rel = PathBuf::from(&file.key.path);
            let _ = spawn("git-review", move || {
                let Ok(review) = crate::ui::editor::review::Repo::open(&git.top) else { return };
                if let Ok(Some(reviewed)) = review.reviewed_text(&rel) {
                    let folded = crate::ui::editor::review::fold_edit(&reviewed, &before, &text);
                    if folded != reviewed
                        && let Err(e) = review.set_reviewed(&rel, &folded, &format!("meerora: your edit to {}", rel.display()))
                    {
                        tracing::warn!("cannot record your edit as reviewed: {e}");
                    }
                }
            });
        }
    }

    fn drop_edits(&mut self) {
        if let Some(FileView { content: Content::Text(doc), .. }) = &mut self.file {
            doc.drop_edits();
        }
        if let Some(file) = &mut self.file {
            file.retokenize();
            file.disk_changed = false;
            // The file may have changed on disk meanwhile.
            file.key.stamp = None;
        }
        self.edit = None;
        self.words.clear();
    }

    fn message(&self) -> String {
        let (subject, body) = (self.subject.trim(), self.body.trim());
        if body.is_empty() { subject.to_string() } else { format!("{subject}\n\n{body}") }
    }

    /// An untrusted repository: read-only until you trust it.
    fn read_only(&self) -> bool {
        self.git.as_ref().is_some_and(|g| g.mode == Mode::Untrusted)
    }

    /// Why Commit cannot run now, or None.
    fn commit_blocker(&self) -> Option<&'static str> {
        let status = self.status_ok()?;
        if self.read_only() {
            Some("Read-only until you trust this repository.")
        } else if self.busy() {
            Some("Wait for git.")
        } else if self.dirty() {
            Some("Save or drop your edits first.")
        } else if !self.amend && status.staged().next().is_none() {
            Some("Stage a change to commit it.")
        } else if self.subject.trim().is_empty() {
            Some("Write a subject.")
        } else if self.amend && !status.has_head() {
            Some("There is no commit to amend yet.")
        } else {
            None
        }
    }

    fn commit(&mut self, hooks_confirmed: bool) {
        if self.dirty() {
            self.confirm = Some(Confirm::Unsaved);
            return;
        }
        if let Some(why) = self.commit_blocker() {
            self.note = Some((why.into(), true));
            return;
        }
        let hooks_changed = match (&self.hooks, self.status_ok()) {
            (Some(hooks), Some(status)) => status.files.iter().any(|f| Path::new(&f.path).starts_with(hooks)),
            _ => false,
        };
        if hooks_changed && !hooks_confirmed {
            self.confirm = Some(Confirm::CommitHooks);
            return;
        }
        self.confirm = None;
        let (message, amend) = (self.message(), self.amend);
        let subject = self.subject.trim().to_string();
        self.act("Committing", move |g| g.commit(&message, amend, true).map(|id| Done::Committed { id, subject }));
    }

    fn push(&mut self) {
        let Some(status) = self.status_ok() else { return };
        let Some(branch) = status.branch.clone() else {
            self.note = Some(("HEAD is detached. Check out a branch to push it.".into(), true));
            return;
        };
        if status.upstream.is_none() {
            self.push_upstream_offer();
            return;
        }
        let _ = branch;
        self.start_remote(None);
    }

    /// Offers `git push -u <remote> <branch>` for a branch without upstream.
    fn push_upstream_offer(&mut self) {
        let branch = self.status_ok().and_then(|s| s.branch.clone());
        let remote = if self.remotes.iter().any(|r| r == "origin") { Some("origin".to_string()) } else { self.remotes.first().cloned() };
        match (remote, branch) {
            (Some(remote), Some(branch)) => self.confirm = Some(Confirm::Upstream { remote, branch }),
            (None, _) => self.note = Some(("This repository has no remote. Add one with git remote add, then push.".into(), true)),
            (_, None) => self.note = Some(("HEAD is detached. Check out a branch to push it.".into(), true)),
        }
    }

    fn start_remote(&mut self, upstream: Option<(String, String)>) {
        let Some(git) = &self.git else { return };
        if self.remote.as_mut().is_some_and(|r| r.running()) {
            return;
        }
        let started = match &upstream {
            Some((remote, branch)) => git.push(Some((remote, branch))),
            None => git.push(None),
        };
        self.start_work(started);
    }

    fn fetch(&mut self) {
        let Some(git) = &self.git else { return };
        if self.remote.as_mut().is_some_and(|r| r.running()) {
            return;
        }
        let started = git.fetch();
        self.start_work(started);
    }

    fn start_work(&mut self, started: Result<Remote, String>) {
        match started {
            Ok(remote) => {
                self.remote = Some(remote);
                self.log_open = true;
                self.confirm = None;
            }
            Err(e) => self.note = Some((e, true)),
        }
    }

    // Drawing.

    fn draw(&mut self, ui: &mut Ui, v: &View, head: Rect, body: Rect, now: f64, out: &mut Out) {
        if ui.rect_contains_pointer(body) && ui.input(|i| i.pointer.any_pressed()) {
            out.clicked = true;
        }
        // Read again when the tile or the window gets focus.
        let window_focus = ui.input(|i| i.focused);
        if (v.selected && !self.was_selected) || (window_focus && !self.had_focus) {
            self.refresh = true;
        }
        self.was_selected = v.selected;
        self.had_focus = window_focus;

        self.draw_head(ui, v, head, out);
        if self.picker || self.git.is_none() {
            self.draw_picker(ui, v, body, out);
            return;
        }
        let status = match &self.status {
            None if self.hold.is_some() => {
                let who = self.hold.clone().unwrap_or_default();
                let text = format!("{who} can still write this untrusted repository, so meerora runs no git here until the run has ended.");
                super::super::agent::placeholder(ui, body, "Waiting for the run to end", &text);
                return;
            }
            None => {
                super::super::agent::placeholder(ui, body, "Reading the repository", "One moment.");
                return;
            }
            Some(Err(e)) => {
                let e = e.clone();
                super::super::agent::placeholder(ui, body, "git status failed", &e);
                return;
            }
            Some(Ok(s)) => s.clone(),
        };
        self.keys(ui, v, out);
        let wide = body.width() >= 700.0;
        let (left, right) = if wide {
            let w = (body.width() * 0.3).clamp(250.0, 330.0).floor();
            (Rect::from_min_size(body.min, vec2(w, body.height())), Rect::from_min_max(pos2(body.min.x + w, body.min.y), body.max))
        } else {
            let h = (body.height() * 0.5).clamp(240.0, 420.0).floor();
            (Rect::from_min_size(body.min, vec2(body.width(), h)), Rect::from_min_max(pos2(body.min.x, body.min.y + h), body.max))
        };
        let painter = ui.painter_at(body);
        painter.rect_filled(left, CornerRadius::ZERO, c(BAR_BG));
        if wide {
            vline(&painter, left.max.x - 0.5, left.min.y, left.max.y);
        } else {
            hline(&painter, left.max.y - 0.5, left.min.x, left.max.x);
        }
        let commit_h = COMMIT_H.min(left.height() * 0.6);
        let files_r = Rect::from_min_max(left.min, pos2(left.max.x, left.max.y - commit_h));
        let commit_r = Rect::from_min_max(pos2(left.min.x, left.max.y - commit_h), left.max);
        self.draw_files(ui, v, &status, files_r);
        self.draw_commit(ui, v, &status, commit_r, out);
        let mut diff_r = right;
        if self.log_open && self.remote.is_some() {
            let log = Rect::from_min_max(pos2(right.min.x, right.max.y - LOG_H.min(right.height() * 0.4)), right.max);
            self.draw_log(ui, log);
            diff_r.max.y = log.min.y;
        }
        self.draw_diff(ui, v, diff_r, now, out);
    }

    /// Ctrl+S saves, Esc closes what is open in the tile.
    fn keys(&mut self, ui: &mut Ui, v: &View, out: &mut Out) {
        let edit_focus = self.edit.is_some() && ui.memory(|m| m.has_focus(edit_id(v)));
        if (v.selected || edit_focus)
            && self.dirty()
            && ui.input_mut(|i| i.consume_shortcut(&KeyboardShortcut::new(Modifiers::COMMAND, Key::S)))
        {
            self.save();
        }
        if self.confirm.is_some() || self.edit.is_some() {
            out.focused = true;
            if !edit_focus && ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) && self.confirm.take().is_none() {
                self.edit = None;
            }
        }
    }

    fn draw_head(&mut self, ui: &mut Ui, v: &View, head: Rect, out: &mut Out) {
        let y = head.center().y;
        let x0 = head.min.x + 52.0;
        let mut right = head.max.x - 8.0;
        if let Some(git) = self.git.clone().filter(|_| !self.picker) {
            let running = self.remote.as_ref().is_some_and(|r| r.result.is_none());
            let works: &[(&str, Work)] = if git.mode == Mode::Untrusted { &[] } else { &[("Push", Work::Push), ("Fetch", Work::Fetch)] };
            for &(label, work) in works {
                let w = if work == Work::Push { 64.0 } else { 62.0 };
                let b = Rect::from_min_size(pos2(right - w, y - 13.0), vec2(w, 26.0));
                right = b.min.x - 6.0;
                if b.min.x < x0 + 160.0 {
                    continue;
                }
                let hint = match work {
                    Work::Push => "Push this branch to its upstream",
                    Work::Fetch => "Fetch from the remote and update ahead and behind",
                };
                let busy = running || self.status_ok().is_none();
                if busy {
                    dim_button(ui, b, label, Id::new(("git-head", label, &v.agent, v.n)));
                } else if small_button(ui, b, label).on_hover_text(hint).clicked() {
                    match work {
                        Work::Push => self.push(),
                        Work::Fetch => self.fetch(),
                    }
                }
            }
            let painter = ui.painter();
            // The repository, a click opens the picker.
            let name = fit(painter, &git.name(), theme::ui_semibold(13.5), 180.0);
            let name_w = painter.layout_no_wrap(name.clone(), theme::ui_semibold(13.5), Color32::WHITE).size().x;
            let repo_r = Rect::from_min_size(pos2(x0, y - 13.0), vec2(name_w + 30.0, 26.0));
            let resp =
                ui.interact(repo_r, Id::new(("git-repo", &v.agent, v.n)), Sense::click()).on_hover_text(git.top.display().to_string());
            let painter = ui.painter();
            if resp.hovered() {
                painter.rect_filled(repo_r, CornerRadius::same(7), c(HOVERED));
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
            painter.text(pos2(x0 + 8.0, y), Align2::LEFT_CENTER, &name, theme::ui_semibold(13.5), c(theme::TEXT));
            chevron(painter, pos2(repo_r.max.x - 11.0, y));
            if resp.clicked() {
                self.picker = true;
                out.clicked = true;
            }
            let mut x = repo_r.max.x + 10.0;
            if let Some(status) = self.status_ok() {
                let branch = status.branch.clone().unwrap_or_else(|| "detached HEAD".into());
                let branch = fit(painter, &branch, theme::mono(11.5), (right - x - 20.0).clamp(0.0, 160.0));
                let r = painter.text(pos2(x, y), Align2::LEFT_CENTER, branch, theme::mono(11.5), c(theme::TEXT_2));
                x = r.max.x + 8.0;
                if status.upstream.is_some() && (status.ahead > 0 || status.behind > 0) {
                    let text = format!("↑{} ↓{}", status.ahead, status.behind);
                    let color = if status.behind > 0 { WARN } else { theme::NEEDS_YOU };
                    let r = painter.text(pos2(x, y), Align2::LEFT_CENTER, text, theme::mono(11.5), c(color));
                    x = r.max.x + 8.0;
                } else if status.upstream.is_none() && status.branch.is_some() && x < right - 80.0 {
                    let r = painter.text(pos2(x, y), Align2::LEFT_CENTER, "no upstream", theme::mono(11.0), c(DIM));
                    x = r.max.x + 8.0;
                }
            }
            if git.mode == Mode::Untrusted && right - x > 100.0 {
                let r = pill(painter, pos2(x + 90.0, y), "UNTRUSTED", WARN);
                x = r.max.x + 10.0;
            }
            // Who changed it (D12).
            if let Some(seconds) = v.changed_ago
                && right - x > 90.0
            {
                let text = fit(painter, &format!("Changed by {} {}", v.name, ago(seconds)), theme::ui(12.5), right - x - 26.0);
                painter.circle_filled(pos2(x + 8.0, y), 4.0, c(v.color));
                painter.text(pos2(x + 17.0, y), Align2::LEFT_CENTER, text, theme::ui(12.5), c(theme::TEXT_2));
            }
        } else if let Some((path, _)) = &self.opening {
            let painter = ui.painter();
            let text = fit(painter, &format!("Opening {}", path.display()), theme::ui(13.0), right - x0);
            painter.text(pos2(x0, y), Align2::LEFT_CENTER, text, theme::ui(13.0), c(theme::TEXT_2));
        } else {
            ui.painter().text(pos2(x0, y), Align2::LEFT_CENTER, "Pick a repository", theme::ui(13.0), c(theme::TEXT_2));
        }
    }

    /// Workspace folders, the agent's directory, recent repositories and a path.
    fn draw_picker(&mut self, ui: &mut Ui, v: &View, body: Rect, out: &mut Out) {
        let panel = Rect::from_min_size(body.min + vec2(16.0, 16.0), vec2((body.width() - 32.0).min(620.0), body.height() - 32.0));
        let painter = ui.painter_at(body);
        painter.text(panel.min, Align2::LEFT_TOP, "Open a repository", theme::ui_bold(18.0), c(theme::TEXT));
        if self.git.is_some() {
            let close = Rect::from_min_size(pos2(panel.max.x - 84.0, panel.min.y - 4.0), vec2(84.0, 30.0));
            if button(ui, close, "Close", ButtonStyle::Quiet).clicked() || ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
                self.picker = false;
            }
            out.focused = true;
        }
        let mut y = panel.min.y + 34.0;
        let painter = ui.painter_at(body);
        let sub = painter.layout(
            format!("Pick what {} works on, or type the path of any repository of yours.", v.name),
            theme::ui(13.5),
            c(theme::TEXT_2),
            panel.width(),
        );
        y += sub.size().y;
        painter.galley(pos2(panel.min.x, y - sub.size().y), sub, c(theme::TEXT_2));
        y += 12.0;
        if let Some(e) = &self.open_error {
            let galley = painter.layout(e.clone(), theme::ui(13.0), c(theme::TEXT), panel.width() - 24.0);
            let strip = Rect::from_min_size(pos2(panel.min.x, y), vec2(panel.width(), galley.size().y + 16.0));
            painter.rect(strip, CornerRadius::same(10), ca(DEL, 18), Stroke::new(1.0, ca(DEL, 80)), StrokeKind::Inside);
            painter.galley(strip.min + vec2(12.0, 8.0), galley, c(theme::TEXT));
            y = strip.max.y + 12.0;
        }
        let open_top = self.git.as_ref().map(|g| g.top.clone());
        let home = user_home();
        let mut rows: Vec<(&str, String, String, Option<PathBuf>)> = Vec::new();
        for (raw, checked) in &v.workspace {
            match checked {
                Ok(path) => {
                    let sub =
                        if path.join(".git").exists() { "git repository".to_string() } else { "no .git folder of its own".to_string() };
                    rows.push(("WORKSPACE FOLDERS", meerora_protocol::workspace::shorten(path, home.as_deref()), sub, Some(path.clone())));
                }
                Err(why) => rows.push(("WORKSPACE FOLDERS", raw.clone(), why.clone(), None)),
            }
        }
        if v.workspace.is_empty() {
            rows.push((
                "WORKSPACE FOLDERS",
                "None yet".into(),
                format!("Add folders in Edit agent, step 2, so {} may work on them.", v.name),
                None,
            ));
        }
        if let Some(dir) = &v.agent_dir {
            let sub = if dir.join(".git").exists() { "its own history, with the hardened git" } else { "not a git repository yet" };
            rows.push(("AGENT DIRECTORY", meerora_protocol::workspace::shorten(dir, home.as_deref()), sub.into(), Some(dir.clone())));
        }
        let recent: Vec<PathBuf> =
            v.recent.iter().filter(|p| !rows.iter().any(|r| r.3.as_deref() == Some(p.as_path()))).take(6).cloned().collect();
        for path in recent {
            rows.push(("RECENT", meerora_protocol::workspace::shorten(&path, home.as_deref()), String::new(), Some(path)));
        }
        let mut heading = "";
        for (section, title, sub, path) in rows {
            if y > panel.max.y - 90.0 {
                break;
            }
            if section != heading {
                heading = section;
                y += 6.0;
                ui.painter().text(pos2(panel.min.x, y), Align2::LEFT_TOP, section, theme::mono(10.5), c(theme::TEXT_2));
                y += 20.0;
            }
            let row = Rect::from_min_size(pos2(panel.min.x, y), vec2(panel.width(), 44.0));
            let resp = ui.interact(
                row,
                Id::new(("git-pick", &v.agent, v.n, &title)),
                if path.is_some() { Sense::click() } else { Sense::hover() },
            );
            let painter = ui.painter();
            if resp.hovered() && path.is_some() {
                painter.rect_filled(row, CornerRadius::same(10), c(HOVERED));
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
            let color = if path.is_some() { c(theme::TEXT) } else { c(DIM) };
            let title_text = fit(painter, &title, theme::mono(12.5), row.width() - 100.0);
            painter.text(row.min + vec2(12.0, 13.0), Align2::LEFT_CENTER, title_text, theme::mono(12.5), color);
            let sub_text = fit(painter, &sub, theme::ui(12.0), row.width() - 24.0);
            painter.text(row.min + vec2(12.0, 31.0), Align2::LEFT_CENTER, sub_text, theme::ui(12.0), c(theme::TEXT_2));
            if path.is_some() && path == open_top {
                pill(painter, pos2(row.max.x - 10.0, row.min.y + 13.0), "OPEN", theme::WORKING);
            }
            if resp.clicked()
                && let Some(path) = path
            {
                out.open = Some(path);
            }
            y = row.max.y + 4.0;
        }
        // A typed path.
        let field = Rect::from_min_size(pos2(panel.min.x, (y + 12.0).min(panel.max.y - 40.0)), vec2(panel.width() - 96.0, 36.0));
        let open_b = Rect::from_min_size(pos2(field.max.x + 10.0, field.min.y), vec2(86.0, 36.0));
        let painter = ui.painter();
        painter.rect(field, CornerRadius::same(10), c(FIELD), Stroke::new(1.0, c(FIELD_LINE)), StrokeKind::Inside);
        let id = Id::new(("git-path", &v.agent, v.n));
        let resp = ui.put(
            field.shrink2(vec2(12.0, 9.0)),
            egui::TextEdit::singleline(&mut self.path_input)
                .id(id)
                .frame(egui::Frame::NONE)
                .font(theme::mono(12.5))
                .hint_text("~/code/project or /path/to/a/repository"),
        );
        if resp.has_focus() {
            out.focused = true;
        }
        let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
        let typed = self.path_input.trim().to_string();
        if (button(ui, open_b, "Open", ButtonStyle::Primary).clicked() || enter) && !typed.is_empty() {
            match meerora_protocol::workspace::expand(Path::new(&typed), home.as_deref()) {
                Some(path) => out.open = Some(path),
                None => self.open_error = Some(format!("{typed} is not an absolute path. Write it like ~/code/project.")),
            }
        }
        if self.opening.is_some() {
            ui.painter().text(pos2(panel.min.x, field.max.y + 10.0), Align2::LEFT_TOP, "Opening…", theme::ui(12.5), c(theme::TEXT_2));
        }
    }

    fn draw_files(&mut self, ui: &mut Ui, v: &View, status: &Status, r: Rect) {
        let sections = [Section::Staged, Section::Changes, Section::Untracked];
        let lists: Vec<(Section, Vec<FileStatus>)> =
            sections.iter().map(|s| (*s, status.files.iter().filter(|f| s.holds(f)).take(MAX_FILES).cloned().collect())).collect();
        if lists.iter().all(|(_, l)| l.is_empty()) {
            let text = match status.upstream {
                Some(_) if status.ahead > 0 => format!("Nothing to commit. {} commits wait to be pushed.", status.ahead),
                _ => "Nothing to commit. The work tree matches the last commit.".to_string(),
            };
            super::super::agent::placeholder(ui, r, "Clean", &text);
            return;
        }
        let mut stage: Option<String> = None;
        let mut unstage: Option<String> = None;
        let mut all: Option<bool> = None;
        let mut pick: Option<(Section, String)> = None;
        let busy = self.busy();
        let locked = self.read_only();
        let mut child = ui.new_child(UiBuilder::new().max_rect(r.shrink2(vec2(0.0, 6.0))).layout(egui::Layout::top_down(egui::Align::Min)));
        child.set_clip_rect(r.intersect(ui.clip_rect()));
        egui::ScrollArea::vertical().id_salt(("git-files", &v.agent, v.n)).auto_shrink([false, false]).show(&mut child, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            for (section, files) in &lists {
                if files.is_empty() {
                    continue;
                }
                let (head, _) = ui.allocate_exact_size(vec2(r.width(), SECTION_H), Sense::hover());
                let painter = ui.painter();
                let label = format!("{} {}", section.title(), files.len());
                painter.text(
                    pos2(head.min.x + 14.0, head.center().y + 2.0),
                    Align2::LEFT_CENTER,
                    label,
                    theme::mono(10.5),
                    c(theme::TEXT_2),
                );
                let (text, staging) = if *section == Section::Staged { ("Unstage all", false) } else { ("Stage all", true) };
                let b = Rect::from_min_size(pos2(head.max.x - 10.0 - 84.0, head.center().y - 10.0), vec2(84.0, 22.0));
                if !locked && !busy && link_button(ui, b, text, Id::new(("git-all", &v.agent, v.n, *section))).clicked() {
                    all = Some(staging);
                }
                for f in files {
                    let (row, resp) = ui.allocate_exact_size(vec2(r.width(), FILE_ROW), Sense::click());
                    if !ui.is_rect_visible(row) {
                        continue;
                    }
                    let is_sel = self.selected.as_ref() == Some(&(*section, f.path.clone()));
                    let painter = ui.painter();
                    if is_sel {
                        painter.rect_filled(row.shrink2(vec2(6.0, 1.0)), CornerRadius::same(6), c(SELECTED));
                    } else if resp.hovered() {
                        painter.rect_filled(row.shrink2(vec2(6.0, 1.0)), CornerRadius::same(6), c(HOVERED));
                    }
                    let (letter, color) = letter(*section, f);
                    painter.text(pos2(row.min.x + 16.0, row.center().y), Align2::LEFT_CENTER, letter, theme::mono_medium(11.0), c(color));
                    // + stages, − unstages.
                    let pm = Rect::from_center_size(pos2(row.max.x - 22.0, row.center().y), vec2(22.0, 20.0));
                    let sign = if *section == Section::Staged { "−" } else { "+" };
                    let hint = if *section == Section::Staged { "Unstage" } else { "Stage" };
                    let pm_resp = (!locked).then(|| {
                        ui.interact(pm, Id::new(("git-pm", &v.agent, v.n, *section, &f.path)), Sense::click()).on_hover_text(hint)
                    });
                    let pm_hovered = pm_resp.as_ref().is_some_and(|r| r.hovered());
                    let painter = ui.painter();
                    if !locked && (resp.hovered() || pm_hovered || is_sel) {
                        let fill = if pm_hovered { c([0x2A, 0x30, 0x3D]) } else { c([0x1E, 0x23, 0x2E]) };
                        painter.rect_filled(pm, CornerRadius::same(5), fill);
                        painter.text(pm.center(), Align2::CENTER_CENTER, sign, theme::mono_medium(13.0), c(theme::TEXT));
                    }
                    if pm_resp.as_ref().is_some_and(|r| r.clicked()) && !busy {
                        if *section == Section::Staged {
                            unstage = Some(f.path.clone());
                        } else {
                            stage = Some(f.path.clone());
                        }
                    } else if resp.clicked() {
                        pick = Some((*section, f.path.clone()));
                    }
                    if resp.hovered() || pm_hovered {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                    }
                    // The agent patched this file (D12).
                    let mut x = row.min.x + 32.0;
                    if v.touched.contains(&f.path) {
                        painter.circle_filled(pos2(x + 3.0, row.center().y), 3.0, c(v.color));
                        x += 12.0;
                    }
                    let room = pm.min.x - x - 8.0;
                    let (dir, name) = match f.path.rsplit_once('/') {
                        Some((d, n)) => (format!("{d}/"), n.to_string()),
                        None => (String::new(), f.path.clone()),
                    };
                    let font = theme::mono(12.0);
                    let name_w = painter.layout_no_wrap(name.clone(), font.clone(), Color32::WHITE).size().x;
                    let name_color = if is_sel { c(theme::TEXT) } else { c([0xC9, 0xCE, 0xD9]) };
                    if dir.is_empty() || name_w >= room - 24.0 {
                        let shown = fit(painter, &f.path, font.clone(), room);
                        painter.text(pos2(x, row.center().y), Align2::LEFT_CENTER, shown, font, name_color);
                    } else {
                        let dir = fit(painter, &dir, font.clone(), room - name_w);
                        let d = painter.text(pos2(x, row.center().y), Align2::LEFT_CENTER, dir, font.clone(), c(DIM));
                        painter.text(pos2(d.max.x, row.center().y), Align2::LEFT_CENTER, name, font, name_color);
                    }
                }
                ui.add_space(6.0);
            }
        });
        if let Some(p) = stage {
            if self.selected.as_ref().is_some_and(|(s, sp)| *sp == p && *s != Section::Staged) && !self.dirty() {
                self.selected = Some((Section::Staged, p.clone()));
            }
            self.stage_file(p);
        } else if let Some(p) = unstage {
            self.unstage_file(p);
        } else if let Some(staging) = all {
            if staging { self.stage_all() } else { self.unstage_all() }
        } else if let Some((s, p)) = pick {
            self.select(s, p);
        }
    }

    fn draw_commit(&mut self, ui: &mut Ui, v: &View, status: &Status, r: Rect, out: &mut Out) {
        let painter = ui.painter_at(r);
        hline(&painter, r.min.y + 0.5, r.min.x, r.max.x);
        let x = r.min.x + 14.0;
        let w = r.width() - 28.0;
        let staged = status.staged().count();
        let head = match staged {
            0 => "COMMIT".to_string(),
            1 => "COMMIT 1 FILE".to_string(),
            n => format!("COMMIT {n} FILES"),
        };
        painter.text(pos2(x, r.min.y + 18.0), Align2::LEFT_CENTER, head, theme::mono(10.5), c(theme::TEXT_2));
        if let Some(branch) = &status.branch {
            let text = fit(&painter, &format!("to {branch}"), theme::mono(10.5), w - 110.0);
            painter.text(pos2(r.max.x - 14.0, r.min.y + 18.0), Align2::RIGHT_CENTER, text, theme::mono(10.5), c(DIM));
        }
        let subject_r = Rect::from_min_size(pos2(x, r.min.y + 32.0), vec2(w, 32.0));
        let body_r = Rect::from_min_size(pos2(x, subject_r.max.y + 6.0), vec2(w, (r.max.y - subject_r.max.y - 64.0).max(28.0)));
        for rect in [subject_r, body_r] {
            painter.rect(rect, CornerRadius::same(8), c(FIELD), Stroke::new(1.0, c(FIELD_LINE)), StrokeKind::Inside);
        }
        let (sid, bid) = (Id::new(("git-subject", &v.agent, v.n)), Id::new(("git-body", &v.agent, v.n)));
        let typing = ui.memory(|m| m.has_focus(sid) || m.has_focus(bid));
        let ctrl_enter = typing && ui.input_mut(|i| i.consume_shortcut(&KeyboardShortcut::new(Modifiers::COMMAND, Key::Enter)));
        let s = ui.put(
            subject_r.shrink2(vec2(10.0, 8.0)),
            egui::TextEdit::singleline(&mut self.subject)
                .id(sid)
                .frame(egui::Frame::NONE)
                .font(theme::ui(13.5))
                .hint_text("Summary of the change"),
        );
        let mut body_ui = ui.new_child(UiBuilder::new().max_rect(body_r.shrink2(vec2(10.0, 7.0))));
        egui::ScrollArea::vertical().id_salt(("git-body-scroll", &v.agent, v.n)).auto_shrink([false, false]).show(&mut body_ui, |ui| {
            ui.add(
                egui::TextEdit::multiline(&mut self.body)
                    .id(bid)
                    .frame(egui::Frame::NONE)
                    .font(theme::ui(13.0))
                    .desired_width(body_r.width() - 24.0)
                    .desired_rows(2)
                    .hint_text("Description (optional)"),
            );
        });
        if s.has_focus() || ui.memory(|m| m.has_focus(bid)) {
            out.focused = true;
        }
        let row_y = r.max.y - 26.0;
        let amend_r = Rect::from_min_size(pos2(x, row_y - 11.0), vec2(120.0, 22.0));
        let amend_resp =
            ui.interact(amend_r, Id::new(("git-amend", &v.agent, v.n)), Sense::click()).on_hover_text("Replace the last commit");
        let painter = ui.painter();
        let boxr = Rect::from_min_size(pos2(x, row_y - 7.0), vec2(14.0, 14.0));
        painter.rect(
            boxr,
            CornerRadius::same(3),
            if self.amend { c(theme::NEEDS_YOU) } else { c(FIELD) },
            Stroke::new(1.0, c(FIELD_LINE)),
            StrokeKind::Inside,
        );
        if self.amend {
            check(painter, boxr.center());
        }
        painter.text(pos2(x + 22.0, row_y), Align2::LEFT_CENTER, "Amend last commit", theme::ui(12.5), c(theme::TEXT_2));
        if amend_resp.clicked() && status.has_head() && !self.read_only() {
            self.amend = !self.amend;
            if self.amend
                && self.subject.trim().is_empty()
                && let Some(git) = self.git.clone()
            {
                self.amend_job = Some(spawn("git-message", move || git.last_message()));
            }
        }
        let commit_b = Rect::from_min_size(pos2(r.max.x - 14.0 - 104.0, row_y - 16.0), vec2(104.0, 32.0));
        let label = if self.amend { "Amend" } else { "Commit" };
        let blocker = self.commit_blocker();
        let clicked = match (&self.action, blocker) {
            (Some((what, _)), _) if what == "Committing" => {
                dim_button(ui, commit_b, "Committing…", Id::new(("git-commit", &v.agent, v.n)));
                false
            }
            (_, Some(why)) => {
                dim_button(ui, commit_b, label, Id::new(("git-commit", &v.agent, v.n))).on_hover_text(why);
                false
            }
            _ => button(ui, commit_b, label, ButtonStyle::Primary).on_hover_text("Ctrl+Enter").clicked(),
        };
        if clicked || (ctrl_enter && blocker.is_none()) {
            self.commit(false);
        } else if ctrl_enter && let Some(why) = blocker {
            self.note = Some((why.into(), true));
        }
    }

    fn draw_log(&mut self, ui: &mut Ui, r: Rect) {
        let Some(remote) = &self.remote else { return };
        let painter = ui.painter_at(r);
        painter.rect_filled(r, CornerRadius::ZERO, c(theme::TERMINAL_BG));
        hline(&painter, r.min.y + 0.5, r.min.x, r.max.x);
        let (state, color) = match (&remote.result, remote.work) {
            (None, Work::Push) => ("PUSHING…".to_string(), theme::WORKING),
            (None, Work::Fetch) => ("FETCHING…".to_string(), theme::WORKING),
            (Some(Ok(())), work) => (format!("{} DONE", work.label().to_uppercase()), ADD),
            (Some(Err(_)), work) => (format!("{} FAILED", work.label().to_uppercase()), DEL),
        };
        painter.text(pos2(r.min.x + 14.0, r.min.y + 16.0), Align2::LEFT_CENTER, state, theme::mono(10.5), c(color));
        let b = Rect::from_min_size(pos2(r.max.x - 12.0 - 64.0, r.min.y + 5.0), vec2(64.0, 22.0));
        let running = remote.result.is_none();
        if link_button(ui, b, if running { "Stop" } else { "Close" }, Id::new(("git-log-close", r.min.x as i32))).clicked() {
            if running {
                remote.stop();
            } else {
                self.log_open = false;
            }
        }
        let mut text = remote.text();
        if let Some(Err(e)) = &remote.result {
            text = format!("{}\n{e}", text.trim_end());
        }
        let painter = ui.painter_at(r);
        let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
        let fits = ((r.height() - 34.0) / 15.0).floor().max(1.0) as usize;
        let mut y = r.min.y + 32.0;
        for (i, line) in lines.iter().skip(lines.len().saturating_sub(fits)).enumerate() {
            let last_error = remote.result.as_ref().is_some_and(|r| r.is_err()) && i + 1 == lines.len().min(fits);
            let color = if last_error { c(DEL) } else { c([0xA9, 0xAF, 0xBC]) };
            let shown = fit(&painter, line, theme::mono(11.0), r.width() - 28.0);
            painter.text(pos2(r.min.x + 14.0, y), Align2::LEFT_TOP, shown, theme::mono(11.0), color);
            y += 15.0;
        }
    }

    fn draw_diff(&mut self, ui: &mut Ui, v: &View, r: Rect, _now: f64, out: &mut Out) {
        if r.width() < 80.0 || r.height() < DIFF_HEAD + 30.0 {
            return;
        }
        let Some((section, path)) = self.selected.clone() else {
            super::super::agent::placeholder(ui, r, "Nothing selected", "Pick a file on the left.");
            return;
        };
        let head = Rect::from_min_size(r.min, vec2(r.width(), DIFF_HEAD));
        let painter = ui.painter_at(r);
        hline(&painter, head.max.y - 0.5, r.min.x, r.max.x);
        // Buttons from the right: the view toggle, then what applies to the file.
        let mut right = head.max.x - 10.0;
        let seg = Rect::from_min_size(pos2(right - 168.0, head.center().y - 14.0), vec2(168.0, 28.0));
        right = seg.min.x - 8.0;
        painter.rect(seg, CornerRadius::same(9), c(BAR_BG), Stroke::new(1.0, c(theme::BORDER)), StrokeKind::Inside);
        let half = Rect::from_min_size(seg.min + vec2(2.0, 2.0), vec2(seg.width() / 2.0 - 2.0, seg.height() - 4.0));
        if seg_button(ui, half, "Side by side", !self.unified) {
            self.unified = false;
        }
        if seg_button(ui, half.translate(vec2(half.width(), 0.0)), "Unified", self.unified) {
            self.unified = true;
        }
        let dirty = self.dirty();
        let busy = self.busy();
        let entry = self.status_ok().and_then(|s| s.file(&path)).cloned();
        let mut buttons: Vec<(&str, FileButton)> = Vec::new();
        if self.read_only() {
            // Nothing changes an untrusted repository.
        } else if dirty {
            buttons.push(("Save", FileButton::Save));
            buttons.push(("Drop edits", FileButton::Drop));
        } else {
            match section {
                Section::Staged => buttons.push(("Unstage file", FileButton::Unstage)),
                Section::Changes => {
                    buttons.push(("Stage file", FileButton::Stage));
                    if entry.as_ref().is_some_and(|e| e.kind == Kind::Tracked) {
                        buttons.push(("Discard", FileButton::Discard));
                    }
                }
                Section::Untracked => buttons.push(("Stage file", FileButton::Stage)),
            }
        }
        let mut pressed = None;
        for (label, what) in &buttons {
            let w = ui.painter().layout_no_wrap(label.to_string(), theme::ui(13.0), Color32::WHITE).size().x + 22.0;
            let b = Rect::from_min_size(pos2(right - w, head.center().y - 14.0), vec2(w, 28.0));
            if b.min.x < head.min.x + 120.0 {
                break;
            }
            right = b.min.x - 6.0;
            let primary = matches!(what, FileButton::Save);
            if busy && !matches!(what, FileButton::Save | FileButton::Drop) {
                dim_button(ui, b, label, Id::new(("git-file-b", &v.agent, v.n, *label)));
            } else if (if primary { button(ui, b, label, ButtonStyle::Primary) } else { small_button(ui, b, label) }).clicked() {
                pressed = Some(*what);
            }
        }
        let painter = ui.painter_at(r);
        let badge = match (section, entry.as_ref()) {
            (Section::Untracked, _) => "NEW FILE",
            (Section::Staged, Some(e)) if e.x == 'A' => "STAGED, NEW",
            (Section::Staged, Some(e)) if e.x == 'D' => "STAGED, DELETED",
            (Section::Staged, _) => "STAGED",
            (Section::Changes, Some(e)) if e.kind == Kind::Conflict => "CONFLICT",
            (Section::Changes, Some(e)) if e.y == 'D' => "DELETED",
            (Section::Changes, _) => "CHANGES",
        };
        let badge = if dirty { "UNSAVED" } else { badge };
        let badge_w = painter.layout_no_wrap(badge.to_string(), theme::mono(10.5), Color32::WHITE).size().x + 14.0;
        let path_text = fit(&painter, &path, theme::mono(12.0), right - head.min.x - 28.0 - badge_w - 8.0);
        let pr = painter.text(pos2(head.min.x + 14.0, head.center().y), Align2::LEFT_CENTER, path_text, theme::mono(12.0), c(theme::TEXT));
        let badge_color = if dirty {
            WARN
        } else if section == Section::Staged {
            theme::NEEDS_YOU
        } else {
            DIM
        };
        pill(&painter, pos2(pr.max.x + 8.0 + badge_w, head.center().y), badge, badge_color);
        match pressed {
            Some(FileButton::Save) => self.save(),
            Some(FileButton::Drop) => self.drop_edits(),
            Some(FileButton::Stage) => {
                if section != Section::Staged {
                    self.selected = Some((Section::Staged, path.clone()));
                }
                self.stage_file(path.clone());
            }
            Some(FileButton::Unstage) => self.unstage_file(path.clone()),
            Some(FileButton::Discard) => self.confirm = Some(Confirm::DiscardFile(path.clone())),
            None => {}
        }

        let mut top = head.max.y;
        // A question, a note or a warning between the head and the rows.
        if let Some(confirm) = self.confirm.clone() {
            let strip = Rect::from_min_max(pos2(r.min.x + 10.0, top + 6.0), pos2(r.max.x - 10.0, top + STRIP_H - 2.0));
            self.draw_confirm(ui, v, &confirm, strip, &path);
            top += STRIP_H;
        } else if let Some((text, error)) = self.note.clone().or_else(|| {
            self.file.as_ref().filter(|f| f.disk_changed).map(|_| {
                ("The file changed on disk while you edited it. Save overwrites that change, Drop edits loads it.".to_string(), true)
            })
        }) {
            let strip = Rect::from_min_max(pos2(r.min.x + 10.0, top + 6.0), pos2(r.max.x - 10.0, top + STRIP_H - 2.0));
            let color = if error { DEL } else { ADD };
            let painter = ui.painter_at(r);
            painter.rect(strip, CornerRadius::same(10), ca(color, 18), Stroke::new(1.0, ca(color, 80)), StrokeKind::Inside);
            let galley = painter.layout(text, theme::ui(13.0), c(theme::TEXT), strip.width() - 54.0);
            painter.galley(pos2(strip.min.x + 12.0, strip.center().y - galley.size().y / 2.0), galley, c(theme::TEXT));
            let x = Rect::from_center_size(pos2(strip.max.x - 18.0, strip.center().y), vec2(22.0, 22.0));
            if link_button(ui, x, "×", Id::new(("git-note-x", &v.agent, v.n))).clicked() {
                self.note = None;
            }
            top += STRIP_H;
        } else if let Some(warning) = self.warning.clone() {
            let strip = Rect::from_min_max(pos2(r.min.x + 10.0, top + 6.0), pos2(r.max.x - 10.0, top + STRIP_H - 2.0));
            let painter = ui.painter_at(r);
            painter.rect(strip, CornerRadius::same(10), ca(WARN, 14), Stroke::new(1.0, ca(WARN, 70)), StrokeKind::Inside);
            let galley = painter.layout(warning, theme::ui(12.5), c(theme::TEXT), strip.width() - 24.0);
            painter.galley(pos2(strip.min.x + 12.0, strip.center().y - galley.size().y / 2.0), galley, c(theme::TEXT));
            top += STRIP_H;
        }
        if self.read_only() {
            let strip = Rect::from_min_max(pos2(r.min.x + 10.0, top + 6.0), pos2(r.max.x - 10.0, top + STRIP_H + 8.0));
            self.draw_untrusted(ui, v, strip);
            top += STRIP_H + 10.0;
        }
        let area = Rect::from_min_max(pos2(r.min.x, top), r.max);
        if area.height() < 30.0 {
            return;
        }
        if let Some(text) = self.show_config.clone() {
            self.draw_config(ui, v, area, &text);
            return;
        }
        let Some(file) = self.file.as_ref().filter(|f| f.key.section == section && f.key.path == path) else {
            super::super::agent::placeholder(ui, area, "Reading the diff", "One moment.");
            return;
        };
        match &file.content {
            Content::Note(title, text) => {
                let (title, text) = (*title, text.clone());
                super::super::agent::placeholder(ui, area, title, &text);
            }
            Content::Text(doc) if doc.hunks.is_empty() => {
                let text = if file.editable { "Click a line on the right to edit it." } else { "The two sides are the same." };
                super::super::agent::placeholder(ui, area, "No changes", text);
            }
            Content::Text(_) => self.draw_rows(ui, v, area, out),
        }
    }

    /// The note on an untrusted repository with Show .git/config and Trust.
    fn draw_untrusted(&mut self, ui: &mut Ui, v: &View, strip: Rect) {
        let painter = ui.painter_at(strip.expand(2.0));
        painter.rect(strip, CornerRadius::same(10), ca(WARN, 16), Stroke::new(1.0, ca(WARN, 80)), StrokeKind::Inside);
        let who = v.made_by.clone().unwrap_or_else(|| "an agent".into());
        let text =
            format!("This repository appeared while {who} could write here. Check .git/config and .gitattributes before you trust it.");
        let trust_label = "Trust this repository";
        let show_label = if self.show_config.is_some() { "Hide .git/config" } else { "Show .git/config" };
        let width = |label: &str, font: FontId| ui.painter().layout_no_wrap(label.to_string(), font, Color32::WHITE).size().x + 26.0;
        let trust_b = Rect::from_min_size(
            pos2(strip.max.x - 8.0 - width(trust_label, theme::ui(14.5)), strip.center().y - 15.0),
            vec2(width(trust_label, theme::ui(14.5)), 30.0),
        );
        let show_b = Rect::from_min_size(
            pos2(trust_b.min.x - 8.0 - width(show_label, theme::ui(14.5)), trust_b.min.y),
            vec2(width(show_label, theme::ui(14.5)), 30.0),
        );
        let galley = painter.layout(text, theme::ui(13.0), c(theme::TEXT), (show_b.min.x - strip.min.x - 22.0).max(60.0));
        painter.galley(pos2(strip.min.x + 12.0, strip.center().y - galley.size().y / 2.0), galley, c(theme::TEXT));
        if button(ui, show_b, show_label, ButtonStyle::Quiet).clicked() {
            self.show_config = match self.show_config {
                Some(_) => None,
                None => self.git.as_ref().map(Git::config_text),
            };
        }
        if button(ui, trust_b, trust_label, ButtonStyle::Quiet).clicked() {
            self.confirm = Some(Confirm::Trust);
        }
    }

    /// The configuration and attribute files of an untrusted repository, read-only.
    fn draw_config(&mut self, ui: &mut Ui, v: &View, area: Rect, text: &str) {
        let panel = area.shrink(10.0);
        ui.painter_at(area).rect(
            panel,
            CornerRadius::same(10),
            c(theme::TERMINAL_BG),
            Stroke::new(1.0, c(theme::BORDER)),
            StrokeKind::Inside,
        );
        let mut child = ui.new_child(UiBuilder::new().max_rect(panel.shrink(12.0)).layout(egui::Layout::top_down(egui::Align::Min)));
        child.set_clip_rect(panel.intersect(ui.clip_rect()));
        egui::ScrollArea::both().id_salt(("git-config", &v.agent, v.n)).auto_shrink([false, false]).show(&mut child, |ui| {
            ui.add(egui::Label::new(egui::RichText::new(text).font(theme::mono(12.0)).color(c([0xC9, 0xCE, 0xD9]))).extend());
        });
    }

    /// Moves the repository to the trusted ones and opens it again with your own git.
    fn trust(&mut self) {
        let Some(git) = &self.git else { return };
        match meerora_protocol::workspace::trust_git_dir(&self.data_dir, &git.trust_key()) {
            Ok(()) => {
                self.note = Some((format!("You trust {} now. Your own git runs here.", git.name()), false));
                self.show_config = None;
                self.reopen = true;
            }
            Err(e) => self.note = Some((format!("Cannot record the trust ({e})."), true)),
        }
    }

    fn draw_confirm(&mut self, ui: &mut Ui, v: &View, confirm: &Confirm, strip: Rect, path: &str) {
        let painter = ui.painter_at(strip.expand(2.0));
        let danger = matches!(confirm, Confirm::DiscardHunk(_) | Confirm::DiscardFile(_));
        let color = if danger { DEL } else { theme::NEEDS_YOU };
        painter.rect(strip, CornerRadius::same(10), ca(color, 16), Stroke::new(1.0, ca(color, 80)), StrokeKind::Inside);
        let (text, yes) = match confirm {
            Confirm::DiscardHunk(_) => (
                "Discard this change? Its lines go back to the version in the index. This cannot be undone, nothing goes to the trash."
                    .to_string(),
                "Discard",
            ),
            Confirm::DiscardFile(p) => (
                format!("Discard every unstaged change to {p}? It goes back to the version in the index. This cannot be undone."),
                "Discard",
            ),
            Confirm::CommitHooks => (
                format!(
                    "Hooks in {} changed and run on your computer when you commit. Agents can edit them. Commit anyway?",
                    self.hooks.as_deref().unwrap_or("the work tree")
                ),
                "Commit",
            ),
            Confirm::Upstream { remote, branch } => {
                (format!("{branch} has no upstream yet. Push it to {remote} and track it there?"), "Push")
            }
            Confirm::Unsaved => (format!("You have unsaved edits in {path}. Save or drop them first."), "Save"),
            Confirm::Trust => (
                format!(
                    "Trust {}? meerora then runs your own git here, with this repository's configuration, hooks and filters.",
                    self.git.as_ref().map_or_else(String::new, Git::name)
                ),
                "Trust",
            ),
        };
        let yes_w = ui.painter().layout_no_wrap(yes.to_string(), theme::ui_bold(14.5), Color32::WHITE).size().x + 28.0;
        let yes_b = Rect::from_min_size(pos2(strip.max.x - 8.0 - yes_w, strip.center().y - 15.0), vec2(yes_w, 30.0));
        let no_label = if *confirm == Confirm::Unsaved { "Drop edits" } else { "Cancel" };
        let no_w = ui.painter().layout_no_wrap(no_label.to_string(), theme::ui(14.5), Color32::WHITE).size().x + 26.0;
        let no_b = Rect::from_min_size(pos2(yes_b.min.x - 8.0 - no_w, yes_b.min.y), vec2(no_w, 30.0));
        let galley = painter.layout(text, theme::ui(13.0), c(theme::TEXT), (no_b.min.x - strip.min.x - 22.0).max(40.0));
        painter.galley(pos2(strip.min.x + 12.0, strip.center().y - galley.size().y / 2.0), galley, c(theme::TEXT));
        if button(ui, no_b, no_label, ButtonStyle::Quiet).clicked() {
            if *confirm == Confirm::Unsaved {
                self.drop_edits();
            }
            self.confirm = None;
        }
        if button(ui, yes_b, yes, ButtonStyle::Primary).clicked() {
            self.confirm = None;
            match confirm.clone() {
                Confirm::DiscardHunk(h) => self.hunk(h, HunkAction::Discard),
                Confirm::DiscardFile(p) => {
                    self.act("Discarding", move |g| g.discard(&p).map(|()| Done::Note(format!("Discarded the changes to {p}"))))
                }
                Confirm::CommitHooks => self.commit(true),
                Confirm::Upstream { remote, branch } => self.start_remote(Some((remote, branch))),
                Confirm::Unsaved => self.save(),
                Confirm::Trust => self.trust(),
            }
        }
        let _ = v;
    }

    /// The hunks of the shown file as rows, side by side or unified, with
    /// the line you edit as a text field.
    fn draw_rows(&mut self, ui: &mut Ui, v: &View, r: Rect, out: &mut Out) {
        let font = theme::mono(FONT);
        let cw = ui.fonts_mut(|f| f.glyph_width(&font, '0')).max(1.0);
        let font_h = ui.fonts_mut(|f| f.row_height(&font));
        let id = edit_id(v);
        let (section, unified) = match &self.file {
            Some(f) => (f.key.section, self.unified),
            None => return,
        };
        // Keys of the edited line come before its text field sees them.
        self.edit_keys(ui, id);

        let Some(file) = &self.file else { return };
        let Some(doc) = file.doc() else { return };
        let editable = file.editable;
        let digits = doc.base.len().max(doc.work.len()).max(1).to_string().len().max(3) as f32;
        let num_w = digits * cw + 14.0;
        // The rows: a header per hunk, then its lines.
        let mut items: Vec<Item> = Vec::new();
        for h in 0..doc.hunks.len() {
            items.push(Item::Head(h));
            if unified {
                items.extend(doc.hunks[h].lines.clone().map(Item::Line));
            } else {
                items.extend(doc.pairs(h).into_iter().map(Item::Pair));
            }
        }
        let mut ys = Vec::with_capacity(items.len() + 1);
        let mut y = 6.0;
        for item in &items {
            ys.push(y);
            y += if matches!(item, Item::Head(_)) { HUNK_H } else { ROW_H };
        }
        let total = y + 16.0;
        let text_w = if unified { r.width() - gutter(num_w, cw, true) - 8.0 } else { r.width() / 2.0 - gutter(num_w, cw, false) - 8.0 };
        let max_h = (file.widest as f32 * cw + 40.0 - text_w).max(0.0);
        if ui.rect_contains_pointer(r) {
            let dx = ui.input(|i| i.smooth_scroll_delta.x);
            self.hscroll = (self.hscroll - dx).clamp(0.0, max_h);
        }
        self.hscroll = self.hscroll.min(max_h);
        let hscroll = self.hscroll;
        let busy = self.busy();
        let dirty = self.dirty();
        let locked = self.read_only();
        let edit_line = self.edit.as_ref().map(|e| e.line);
        let mut clicked_line: Option<(usize, usize)> = None;
        let mut hunk_action: Option<(usize, HunkAction)> = None;
        let mut edit_changed = false;
        let mut edit_lost = false;
        let mut child = ui.new_child(UiBuilder::new().max_rect(r).layout(egui::Layout::top_down(egui::Align::Min)));
        child.set_clip_rect(r.intersect(ui.clip_rect()));
        let words = &mut self.words;
        let edit = &mut self.edit;
        egui::ScrollArea::vertical()
            .id_salt(("git-rows", &v.agent, v.n, &file.key.path, section_key(section)))
            .auto_shrink([false, false])
            .content_margin(egui::Margin::ZERO)
            .show_viewport(&mut child, |ui, viewport| {
                ui.spacing_mut().item_spacing = egui::Vec2::ZERO;
                let (all, _) = ui.allocate_exact_size(vec2(r.width(), total), Sense::hover());
                let clip = ui.clip_rect();
                let first = ys.partition_point(|y| *y + HUNK_H < viewport.min.y);
                let last = ys.partition_point(|y| *y <= viewport.max.y + ROW_H).min(items.len());
                for i in first..last {
                    let y = all.min.y + ys[i];
                    match items[i] {
                        Item::Head(h) => {
                            let row = Rect::from_x_y_ranges(clip.min.x..=clip.max.x, y + 4.0..=y + HUNK_H - 2.0);
                            if let Some(a) = hunk_head(ui, v, doc, h, row, section, busy, dirty, locked) {
                                hunk_action = Some((h, a));
                            }
                        }
                        Item::Pair(pair) => {
                            let row = Rect::from_x_y_ranges(clip.min.x..=clip.max.x, y..=y + ROW_H);
                            let mid = (row.min.x + row.width() / 2.0).floor();
                            let left = Rect::from_x_y_ranges(row.min.x..=mid, row.y_range());
                            let right = Rect::from_x_y_ranges(mid + 1.0..=row.max.x, row.y_range());
                            let pair_words = match (pair.old, pair.new) {
                                (Some(o), Some(n)) if pair.changed => Some(word_ranges(words, doc, o, n)),
                                _ => None,
                            };
                            let lkind = match pair.old {
                                Some(_) if pair.changed => Shade::Removed,
                                Some(_) => Shade::Plain,
                                None => Shade::Filler,
                            };
                            let rkind = match pair.new {
                                Some(_) if pair.changed => Shade::Added,
                                Some(_) => Shade::Plain,
                                None => Shade::Filler,
                            };
                            let ui_painter = ui.painter().clone();
                            let paint = Cell { font: &font, cw, num_w, hscroll };
                            let lw = pair_words.as_ref().map(|w| w.0.as_slice()).unwrap_or(&[]);
                            let base = pair.old.map(|o| (content(&doc.base[o]), token_line(&file.base_tokens, o)));
                            paint.cell(&ui_painter, left, pair.old, None, lkind, base, lw, ui);
                            vline(&ui_painter, mid + 0.5, row.min.y, row.max.y);
                            let rw = pair_words.as_ref().map(|w| w.1.as_slice()).unwrap_or(&[]);
                            let editing = pair.new.is_some() && pair.new == edit_line;
                            let work = pair.new.filter(|_| !editing).map(|n| (content(&doc.work[n]), token_line(&file.work_tokens, n)));
                            paint.cell(&ui_painter, right, pair.new, None, rkind, work, rw, ui);
                            if let Some(n) = pair.new {
                                let text_r = Rect::from_x_y_ranges(right.min.x + gutter(num_w, cw, false)..=right.max.x, right.y_range());
                                if editing && let Some(e) = edit.as_mut() {
                                    let (changed, lost) = edit_field(ui, id, e, text_r, &font, font_h, file.lang);
                                    edit_changed |= changed;
                                    edit_lost |= lost;
                                } else if editable && section != Section::Staged {
                                    let resp = ui.interact(text_r, Id::new(("git-cell", &v.agent, v.n, n)), Sense::click());
                                    if resp.hovered() {
                                        ui.ctx().set_cursor_icon(egui::CursorIcon::Text);
                                    }
                                    if resp.clicked() {
                                        let px = resp.interact_pointer_pos().map_or(0.0, |p| p.x - text_r.min.x + hscroll);
                                        clicked_line =
                                            Some((n, char_at_column(content(&doc.work[n]), (px / cw).round().max(0.0) as usize)));
                                    }
                                }
                            }
                        }
                        Item::Line(l) => {
                            let line = doc.lines[l];
                            let row = Rect::from_x_y_ranges(clip.min.x..=clip.max.x, y..=y + ROW_H);
                            let shade = match line.tag {
                                Tag::Equal => Shade::Plain,
                                Tag::Delete => Shade::Removed,
                                Tag::Insert => Shade::Added,
                            };
                            let lw: Vec<Range<usize>> = match (line.tag, line.old, line.new, line.partner) {
                                (Tag::Delete, Some(o), _, Some(n)) => word_ranges(words, doc, o, n).0,
                                (Tag::Insert, _, Some(n), Some(o)) => word_ranges(words, doc, o, n).1,
                                _ => Vec::new(),
                            };
                            let editing = line.new.is_some() && line.new == edit_line;
                            let text = match (line.tag, line.old, line.new) {
                                (Tag::Delete, Some(o), _) => Some((content(&doc.base[o]), token_line(&file.base_tokens, o))),
                                (_, _, Some(n)) if !editing => Some((content(&doc.work[n]), token_line(&file.work_tokens, n))),
                                _ => None,
                            };
                            let ui_painter = ui.painter().clone();
                            let paint = Cell { font: &font, cw, num_w, hscroll };
                            paint.cell(&ui_painter, row, line.old, Some(line.new), shade, text, &lw, ui);
                            if let Some(n) = line.new {
                                let text_r = Rect::from_x_y_ranges(row.min.x + gutter(num_w, cw, true)..=row.max.x, row.y_range());
                                if editing && let Some(e) = edit.as_mut() {
                                    let (changed, lost) = edit_field(ui, id, e, text_r, &font, font_h, file.lang);
                                    edit_changed |= changed;
                                    edit_lost |= lost;
                                } else if editable && section != Section::Staged {
                                    let resp = ui.interact(text_r, Id::new(("git-cell", &v.agent, v.n, n)), Sense::click());
                                    if resp.hovered() {
                                        ui.ctx().set_cursor_icon(egui::CursorIcon::Text);
                                    }
                                    if resp.clicked() {
                                        let px = resp.interact_pointer_pos().map_or(0.0, |p| p.x - text_r.min.x + hscroll);
                                        clicked_line =
                                            Some((n, char_at_column(content(&doc.work[n]), (px / cw).round().max(0.0) as usize)));
                                    }
                                }
                            }
                        }
                    }
                }
            });
        if edit_changed {
            self.edit_applied();
        }
        if edit_lost && clicked_line.is_none() {
            self.end_edit();
        }
        if let Some((line, col)) = clicked_line {
            self.start_edit(line, col);
        }
        if self.edit.is_some() {
            out.focused = true;
        }
        if let Some((h, action)) = hunk_action {
            match action {
                HunkAction::Discard => self.confirm = Some(Confirm::DiscardHunk(h)),
                other => self.hunk(h, other),
            }
        }
    }

    fn start_edit(&mut self, line: usize, col: usize) {
        let Some(FileView { content: Content::Text(doc), .. }) = &mut self.file else { return };
        let Some(text) = doc.work.get(line).map(|l| content(l).to_string()) else { return };
        doc.keep(Some(line));
        self.edit = Some(Edit { line, text, cursor: Some(col) });
    }

    fn end_edit(&mut self) {
        self.edit = None;
        if let Some(FileView { content: Content::Text(doc), .. }) = &mut self.file {
            doc.keep(None);
        }
    }

    /// The text field changed: the buffer takes it and aligns again.
    fn edit_applied(&mut self) {
        let Some(edit) = &mut self.edit else { return };
        let Some(file) = &mut self.file else { return };
        let Content::Text(doc) = &mut file.content else { return };
        let (line, col) = doc.set_line(edit.line, &edit.text);
        if line != edit.line {
            edit.line = line;
            edit.text = doc.work.get(line).map(|l| content(l).to_string()).unwrap_or_default();
            edit.cursor = Some(col);
        }
        doc.keep(Some(edit.line));
        file.retokenize();
        self.words.clear();
    }

    /// Enter, the arrows, Backspace at the start, Delete at the end, Tab and
    /// Esc while you edit a line.
    fn edit_keys(&mut self, ui: &mut Ui, id: Id) {
        if self.edit.is_none() || !ui.memory(|m| m.has_focus(id)) {
            return;
        }
        let col = egui::text_edit::TextEditState::load(ui.ctx(), id)
            .and_then(|s| s.cursor.char_range())
            .map(|r| (r.primary.index.0, r.secondary.index.0));
        let (cursor, anchor) = col.unwrap_or((0, 0));
        let Some(edit) = self.edit.as_mut() else { return };
        let Some(FileView { content: Content::Text(doc), .. }) = &mut self.file else { return };
        let len = edit.text.chars().count();
        let pressed = |ui: &mut Ui, key: Key| ui.input_mut(|i| i.consume_key(Modifiers::NONE, key));
        let mut changed = false;
        if pressed(ui, Key::Enter) {
            doc.set_line(edit.line, &edit.text);
            edit.line = doc.split_line(edit.line, cursor);
            edit.text = content(&doc.work[edit.line]).to_string();
            edit.cursor = Some(0);
            changed = true;
        } else if pressed(ui, Key::ArrowUp) && edit.line > 0 {
            edit.line -= 1;
            edit.text = content(&doc.work[edit.line]).to_string();
            edit.cursor = Some(cursor.min(edit.text.chars().count()));
        } else if pressed(ui, Key::ArrowDown) && edit.line + 1 < doc.work.len() {
            edit.line += 1;
            edit.text = content(&doc.work[edit.line]).to_string();
            edit.cursor = Some(cursor.min(edit.text.chars().count()));
        } else if cursor == 0 && anchor == 0 && edit.line > 0 && pressed(ui, Key::Backspace) {
            doc.set_line(edit.line, &edit.text);
            if let Some((line, at)) = doc.join_previous(edit.line) {
                edit.line = line;
                edit.text = content(&doc.work[line]).to_string();
                edit.cursor = Some(at);
                changed = true;
            }
        } else if cursor == len && anchor == len && edit.line + 1 < doc.work.len() && pressed(ui, Key::Delete) {
            doc.set_line(edit.line, &edit.text);
            if doc.join_next(edit.line) {
                edit.text = content(&doc.work[edit.line]).to_string();
                edit.cursor = Some(cursor);
                changed = true;
            }
        } else if pressed(ui, Key::Tab) {
            let indent = if doc.work.iter().any(|l| l.starts_with('\t')) { "\t" } else { "    " };
            let at = edit.text.char_indices().nth(cursor.min(anchor)).map_or(edit.text.len(), |(i, _)| i);
            edit.text.insert_str(at, indent);
            edit.cursor = Some(cursor.min(anchor) + indent.chars().count());
            doc.set_line(edit.line, &edit.text);
            changed = true;
        } else if pressed(ui, Key::Escape) {
            self.end_edit();
            return;
        }
        let line = edit.line;
        doc.keep(Some(line));
        if changed && let Some(file) = &mut self.file {
            file.retokenize();
            self.words.clear();
        }
    }
}

/// The file to show when none is picked: the first change, else the first
/// new file, else the first staged one.
fn first(status: &Status) -> Option<(Section, String)> {
    [Section::Changes, Section::Untracked, Section::Staged]
        .into_iter()
        .find_map(|s| status.files.iter().find(|f| s.holds(f)).map(|f| (s, f.path.clone())))
}

/// The id of the text field of the line you edit.
fn edit_id(v: &View) -> Id {
    Id::new(("git-edit", &v.agent, v.n))
}

fn section_key(s: Section) -> u8 {
    s as u8
}

fn token_line(tokens: &[Tokens], n: usize) -> &Tokens {
    static EMPTY: Tokens = Vec::new();
    tokens.get(n).unwrap_or(&EMPTY)
}

/// The text field of the edited line. Returns (changed, lost focus).
fn edit_field(ui: &mut Ui, id: Id, e: &mut Edit, r: Rect, font: &FontId, font_h: f32, lang: Lang) -> (bool, bool) {
    let field = Rect::from_min_size(pos2(r.min.x, r.center().y - font_h / 2.0), vec2(r.width(), font_h));
    ui.painter().rect_filled(Rect::from_x_y_ranges(r.min.x - 4.0..=r.max.x, r.y_range()), CornerRadius::ZERO, ca(theme::NEEDS_YOU, 16));
    if let Some(col) = e.cursor.take() {
        let mut state = egui::text_edit::TextEditState::load(ui.ctx(), id).unwrap_or_default();
        state.cursor.set_char_range(Some(CCursorRange::one(CCursor::new(col))));
        state.store(ui.ctx(), id);
        ui.memory_mut(|m| m.request_focus(id));
    }
    let font = font.clone();
    let mut layouter = move |ui: &Ui, buf: &dyn egui::TextBuffer, _wrap: f32| {
        let text = buf.as_str();
        let tokens = highlight::tokens(text, lang);
        ui.fonts_mut(|f| f.layout_job(colored_job(text, &tokens, &font, false)))
    };
    let resp = ui.put(
        field,
        egui::TextEdit::singleline(&mut e.text)
            .id(id)
            .frame(egui::Frame::NONE)
            .margin(egui::Margin::ZERO)
            .desired_width(field.width())
            .lock_focus(true)
            .layouter(&mut layouter),
    );
    (resp.changed(), resp.lost_focus())
}

/// Word-level highlights of base line `o` against working line `n`, cached.
fn word_ranges(cache: &mut Words, doc: &Doc, o: usize, n: usize) -> (Vec<Range<usize>>, Vec<Range<usize>>) {
    cache
        .entry((o, n))
        .or_insert_with(|| {
            let (Some(a), Some(b)) = (doc.base.get(o), doc.work.get(n)) else { return (Vec::new(), Vec::new()) };
            changed_ranges(&display(content(a)), &display(content(b)))
        })
        .clone()
}

/// A layout job of one line in syntax colours, tabs as four spaces when
/// `expand` is set.
fn colored_job(text: &str, tokens: &Tokens, font: &FontId, expand: bool) -> LayoutJob {
    let mut job = LayoutJob::default();
    job.wrap.max_width = f32::INFINITY;
    let mut at = 0;
    let push = |job: &mut LayoutJob, piece: &str, kind: highlight::Kind| {
        let piece = if expand { display(piece) } else { piece.to_string() };
        job.append(&piece, 0.0, TextFormat { font_id: font.clone(), color: highlight::color(kind), ..Default::default() });
    };
    for (range, kind) in tokens {
        let (start, end) = (range.start.min(text.len()), range.end.min(text.len()));
        if start < at || !text.is_char_boundary(start) || !text.is_char_boundary(end) {
            continue;
        }
        if start > at {
            push(&mut job, &text[at..start], highlight::Kind::Text);
        }
        push(&mut job, &text[start..end], *kind);
        at = end;
    }
    if at < text.len() || job.sections.is_empty() {
        push(&mut job, &text[at..], highlight::Kind::Text);
    }
    job
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Shade {
    Plain,
    Added,
    Removed,
    Filler,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HunkAction {
    Stage,
    Unstage,
    Discard,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FileButton {
    Save,
    Drop,
    Stage,
    Unstage,
    Discard,
}

#[derive(Clone, Copy)]
enum Item {
    Head(usize),
    Pair(diff::Pair),
    Line(usize),
}

/// Width of the gutter before the text: one or two line numbers, a gap, the
/// sign and another gap.
fn gutter(num_w: f32, cw: f32, unified: bool) -> f32 {
    if unified { 2.0 * num_w + cw + 16.0 } else { num_w + cw + 16.0 }
}

/// Paints one cell of a diff row: the tint, the line numbers and the text.
struct Cell<'a> {
    font: &'a FontId,
    cw: f32,
    num_w: f32,
    hscroll: f32,
}

impl Cell<'_> {
    /// `old` is the line number on the left. `new` is the second number of
    /// a unified row, None for a side-by-side cell.
    #[allow(clippy::too_many_arguments)]
    fn cell(
        &self,
        painter: &egui::Painter,
        r: Rect,
        old: Option<usize>,
        new: Option<Option<usize>>,
        shade: Shade,
        text: Option<(&str, &Tokens)>,
        words: &[Range<usize>],
        ui: &Ui,
    ) {
        let painter = painter.with_clip_rect(r.intersect(painter.clip_rect()));
        let (tint, strong, sign, sign_color) = match shade {
            Shade::Added => (ca(ADD, 24), ca(ADD, 72), "+", c(ADD)),
            Shade::Removed => (ca(DEL, 24), ca(DEL, 72), "−", c(DEL)),
            Shade::Plain => (Color32::TRANSPARENT, Color32::TRANSPARENT, "", c(DIM)),
            Shade::Filler => (c(FILLER), Color32::TRANSPARENT, "", c(DIM)),
        };
        painter.rect_filled(r, CornerRadius::ZERO, tint);
        let unified = new.is_some();
        let gutter_w = gutter(self.num_w, self.cw, unified);
        let text_left = r.min.x + gutter_w - self.hscroll;
        if let Some((line, tokens)) = text {
            let shown = display(line);
            let text_painter =
                painter.with_clip_rect(Rect::from_x_y_ranges(r.min.x + gutter_w..=r.max.x, r.y_range()).intersect(painter.clip_rect()));
            for range in words {
                let col = shown.get(..range.start).map_or(0, |s| s.chars().count()) as f32;
                let len = shown.get(range.clone()).map_or(0, |s| s.chars().count()) as f32;
                let mark = Rect::from_min_size(pos2(text_left + col * self.cw, r.min.y + 1.0), vec2((len * self.cw).max(2.0), ROW_H - 2.0));
                text_painter.rect_filled(mark, CornerRadius::same(2), strong);
            }
            let galley = ui.fonts_mut(|f| f.layout_job(colored_job(line, tokens, self.font, true)));
            text_painter.galley(pos2(text_left, r.center().y - galley.size().y / 2.0), galley, c(theme::TEXT));
        }
        // The gutter keeps its place while the text scrolls sideways.
        let gutter = Rect::from_x_y_ranges(r.min.x..=r.min.x + gutter_w - 4.0, r.y_range());
        painter.rect_filled(gutter, CornerRadius::ZERO, c(theme::PANEL));
        painter.rect_filled(gutter, CornerRadius::ZERO, tint);
        let numbers: Vec<(Option<usize>, f32)> = match new {
            Some(n) => vec![(old, r.min.x + self.num_w), (n, r.min.x + 2.0 * self.num_w)],
            None => vec![(old, r.min.x + self.num_w)],
        };
        for (n, x) in numbers {
            if let Some(n) = n {
                painter.text(pos2(x, r.center().y), Align2::RIGHT_CENTER, (n + 1).to_string(), self.font.clone(), c(NUMBER));
            }
        }
        painter.text(
            pos2(r.min.x + gutter_w - 8.0 - self.cw / 2.0, r.center().y),
            Align2::CENTER_CENTER,
            sign,
            self.font.clone(),
            sign_color,
        );
    }
}

/// The header row of a hunk with its buttons. Returns the button pressed.
#[allow(clippy::too_many_arguments)]
fn hunk_head(
    ui: &mut Ui,
    v: &View,
    doc: &Doc,
    h: usize,
    row: Rect,
    section: Section,
    busy: bool,
    dirty: bool,
    locked: bool,
) -> Option<HunkAction> {
    let painter = ui.painter();
    painter.rect_filled(row, CornerRadius::ZERO, c([0x15, 0x1A, 0x25]));
    let hunk = &doc.hunks[h];
    let label = if hunk.changes { doc.header(h) } else { "editing".to_string() };
    painter.text(pos2(row.min.x + 14.0, row.center().y), Align2::LEFT_CENTER, label, theme::mono(11.5), ca(theme::NEEDS_YOU, 190));
    if !hunk.changes || locked {
        return None;
    }
    let actions: &[(&str, HunkAction)] = match section {
        Section::Staged => &[("Unstage hunk", HunkAction::Unstage)],
        Section::Changes => &[("Discard", HunkAction::Discard), ("Stage hunk", HunkAction::Stage)],
        Section::Untracked => &[],
    };
    let mut right = row.max.x - 10.0;
    let mut pressed = None;
    for (label, action) in actions.iter().rev() {
        let w = ui.painter().layout_no_wrap(label.to_string(), theme::ui(12.5), Color32::WHITE).size().x + 18.0;
        let b = Rect::from_min_size(pos2(right - w, row.center().y - 11.0), vec2(w, 22.0));
        right = b.min.x - 6.0;
        let id = Id::new(("git-hunk", &v.agent, v.n, h, *label));
        if busy || dirty {
            let why = if dirty { "Save or drop your edits first" } else { "Wait for git" };
            dim_link(ui, b, label, id).on_hover_text(why);
        } else if link_button(ui, b, label, id).clicked() {
            pressed = Some(*action);
        }
    }
    pressed
}

/// The status letter of a file in a section and its colour.
fn letter(section: Section, f: &FileStatus) -> (&'static str, [u8; 3]) {
    if f.kind == Kind::Conflict {
        return ("!", DEL);
    }
    if section == Section::Untracked {
        return ("U", ADD);
    }
    let code = if section == Section::Staged { f.x } else { f.y };
    match code {
        'A' => ("A", ADD),
        'D' => ("D", DEL),
        'R' => ("R", theme::NEEDS_YOU),
        'T' => ("T", DIM),
        _ => ("M", WARN),
    }
}

/// A small check mark, the fonts have no glyph for it.
fn check(painter: &egui::Painter, center: egui::Pos2) {
    let stroke = Stroke::new(1.6, c([0x0B, 0x1A, 0x26]));
    painter.line_segment([center + vec2(-4.0, 0.0), center + vec2(-1.0, 3.0)], stroke);
    painter.line_segment([center + vec2(-1.0, 3.0), center + vec2(4.5, -3.5)], stroke);
}

fn chevron(painter: &egui::Painter, center: egui::Pos2) {
    let stroke = Stroke::new(1.4, c(theme::TEXT_2));
    painter.line_segment([center + vec2(-4.0, -2.0), center + vec2(0.0, 2.0)], stroke);
    painter.line_segment([center + vec2(0.0, 2.0), center + vec2(4.0, -2.0)], stroke);
}

/// A compact quiet button for the tile's head and toolbars.
fn small_button(ui: &mut Ui, rect: Rect, label: &str) -> egui::Response {
    let resp = ui.interact(rect, ui.id().with(("git-small", label, rect.min.x as i32, rect.min.y as i32)), Sense::click());
    let hovered = resp.hovered();
    let painter = ui.painter();
    let fill = if hovered { c([0x1A, 0x1F, 0x2A]) } else { Color32::TRANSPARENT };
    let stroke = Stroke::new(1.0, if hovered { c([0x3A, 0x42, 0x52]) } else { c([0x2A, 0x30, 0x3D]) });
    painter.rect(rect, CornerRadius::same(8), fill, stroke, StrokeKind::Inside);
    painter.text(rect.center(), Align2::CENTER_CENTER, label, theme::ui(13.0), c([0xC9, 0xCE, 0xD9]));
    if hovered {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp
}

/// A button that does nothing, drawn dim, for actions that wait.
fn dim_button(ui: &mut Ui, rect: Rect, label: &str, id: Id) -> egui::Response {
    let resp = ui.interact(rect, id, Sense::hover());
    let painter = ui.painter();
    painter.rect(rect, CornerRadius::same(8), Color32::TRANSPARENT, Stroke::new(1.0, c([0x22, 0x27, 0x31])), StrokeKind::Inside);
    painter.text(rect.center(), Align2::CENTER_CENTER, label, theme::ui(13.0), c([0x5A, 0x60, 0x6E]));
    resp
}

/// A text button without a frame until hovered, for rows.
fn link_button(ui: &mut Ui, rect: Rect, label: &str, id: Id) -> egui::Response {
    let resp = ui.interact(rect, id, Sense::click());
    let painter = ui.painter();
    if resp.hovered() {
        painter.rect_filled(rect, CornerRadius::same(6), c([0x22, 0x27, 0x33]));
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let color = if resp.hovered() { c(theme::TEXT) } else { c([0xB9, 0xBF, 0xCC]) };
    painter.text(rect.center(), Align2::CENTER_CENTER, label, theme::ui(12.5), color);
    resp
}

fn dim_link(ui: &mut Ui, rect: Rect, label: &str, id: Id) -> egui::Response {
    let resp = ui.interact(rect, id, Sense::hover());
    ui.painter().text(rect.center(), Align2::CENTER_CENTER, label, theme::ui(12.5), c([0x5A, 0x60, 0x6E]));
    resp
}

/// One half of a segmented toggle. Returns true when clicked.
fn seg_button(ui: &mut Ui, rect: Rect, label: &str, on: bool) -> bool {
    let resp = ui.interact(rect, ui.id().with(("git-seg", label, rect.min.x as i32)), Sense::click());
    let painter = ui.painter();
    if on {
        painter.rect_filled(rect, CornerRadius::same(7), c([0x22, 0x28, 0x34]));
    } else if resp.hovered() {
        painter.rect_filled(rect, CornerRadius::same(7), c(HOVERED));
    }
    let color = if on { c(theme::TEXT) } else { c(theme::TEXT_2) };
    painter.text(rect.center(), Align2::CENTER_CENTER, label, theme::ui(12.5), color);
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp.clicked()
}

// The agent and app standard (D30) and snapshots.

/// What `app_read` returns for Git instance `n` of `agent` (D30,
/// docs/design/agent-apps.md): the repository, the branch, the files by
/// section and the shown file's hunks. None when the instance is not open.
pub fn read(core: &Core, agent: &str, n: u32) -> Option<serde_json::Value> {
    Some(core.ui.git.instances.get(&(agent.to_string(), n))?.reading())
}

impl Instance {
    /// The instance as text for `app_read` (D30).
    fn reading(&self) -> serde_json::Value {
        let inst = self;
        let Some(git) = &inst.git else {
            return serde_json::json!({ "title": "Git", "summary": "No repository is open.", "content": "", "elements": [], "extra": {} });
        };
        let mut content = String::new();
        let mut elements = Vec::new();
        let mut summary = git.name();
        if let Some(status) = inst.status_ok() {
            let (staged, changed, untracked) = (status.staged().count(), status.changed().count(), status.untracked().count());
            summary = format!(
                "{} on {}: {staged} staged, {changed} changed, {untracked} untracked{}",
                git.name(),
                status.branch.as_deref().unwrap_or("a detached HEAD"),
                if status.upstream.is_some() {
                    format!(", {} ahead, {} behind", status.ahead, status.behind)
                } else {
                    ", no upstream".into()
                }
            );
            for section in [Section::Staged, Section::Changes, Section::Untracked] {
                let files: Vec<&FileStatus> = status.files.iter().filter(|f| section.holds(f)).collect();
                if files.is_empty() {
                    continue;
                }
                content.push_str(&format!("{}:\n", section.title().to_lowercase()));
                for f in files {
                    content.push_str(&format!("  {} {}\n", letter(section, f).0, f.path));
                    let value = match section {
                        Section::Staged => "staged",
                        Section::Changes => "changed",
                        Section::Untracked => "untracked",
                    };
                    elements
                        .push(serde_json::json!({ "ref": format!("{value}:{}", f.path), "role": "file", "name": f.path, "value": value }));
                }
            }
        }
        if let Some(file) = &inst.file
            && let Some(doc) = file.doc()
        {
            content.push_str(&format!("\nshown: {} ({:?})\n", file.key.path, file.key.section));
            for h in (0..doc.hunks.len()).filter(|h| doc.hunks[*h].changes) {
                let lines = &doc.lines[doc.hunks[h].lines.clone()];
                let (added, removed) =
                    (lines.iter().filter(|l| l.tag == Tag::Insert).count(), lines.iter().filter(|l| l.tag == Tag::Delete).count());
                content.push_str(&format!("  {} +{added} -{removed}\n", doc.header(h)));
            }
        }
        let status = inst.status_ok();
        serde_json::json!({
            "title": format!("Git · {}", git.name()),
            "summary": summary,
            "content": content,
            "elements": elements,
            "extra": {
                "repository": git.top.display().to_string(),
                "branch": status.and_then(|s| s.branch.clone()),
                "upstream": status.and_then(|s| s.upstream.clone()),
                "ahead": status.map(|s| s.ahead),
                "behind": status.map(|s| s.behind),
            },
        })
    }
}

/// The repository of Git instance `n` of `agent`, for `app_act` through
/// [`act`] on a thread of the caller (agent_apps/git.rs).
pub fn repository(core: &Core, agent: &str, n: u32) -> Option<Git> {
    core.ui.git.instances.get(&(agent.to_string(), n))?.git.clone()
}

/// `app_act` on a Git instance (D30): `stage {path}`, `unstage {path}` and
/// `commit {message}`, see [`repo::agent_act`]. Runs git on the calling thread.
pub fn act(git: &Git, action: &str, args: &serde_json::Value) -> Result<String, String> {
    repo::agent_act(git, action, args)
}

/// What a snapshot shows in Git instance `n` of `agent`: the repository,
/// unified or side by side, a staged file instead of a changed one, and a
/// commit message in the box.
pub struct SnapshotGit {
    pub repo: Option<PathBuf>,
    pub unified: bool,
    pub staged: bool,
    pub message: Option<String>,
}

/// Prepares a Git instance for `meerora snapshot` (`--open git:1=PATH`, `--git`).
pub fn snapshot(core: &mut Core, agent: &str, n: u32, s: &SnapshotGit) {
    let data_dir = core.assets.data_dir.clone();
    let areas = areas(core);
    let inst = core.ui.git.instances.entry((agent.to_string(), n)).or_default();
    inst.unified = s.unified;
    if let Some(message) = &s.message {
        let (subject, body) = message.split_once("\\n").unwrap_or((message.as_str(), ""));
        inst.subject = subject.to_string();
        inst.body = body.replace("\\n", "\n");
    }
    if let Some(repo) = &s.repo {
        inst.start_open(repo.clone(), areas, data_dir);
    }
    if s.staged {
        inst.selected = None;
        inst.prefer_staged = true;
    }
}

/// Waits until every Git instance read its repository and its shown file,
/// for snapshots.
pub fn wait_settled(core: &mut Core, timeout: Duration) {
    let deadline = std::time::Instant::now() + timeout;
    let now = core.world.time;
    while std::time::Instant::now() < deadline {
        let mut busy = false;
        for inst in core.ui.git.instances.values_mut() {
            inst.update(now);
            if inst.prefer_staged
                && let Some(path) = inst.status_ok().and_then(|s| s.staged().next().map(|f| f.path.clone()))
            {
                inst.prefer_staged = false;
                inst.selected = Some((Section::Staged, path));
                inst.file = None;
                inst.schedule_load();
            }
            busy |= inst.opening.is_some() || inst.poll.is_some() || inst.load.is_some() || inst.action.is_some();
            busy |= inst.git.is_some() && inst.status.is_none();
        }
        if !busy {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(test)]
mod tests;
