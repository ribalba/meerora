//! The editor tile of the agent view (D11, D12). Q2 is decided for now with
//! an editor built into the app: a file tree of the agent's directory (D21),
//! egui text editing with line numbers and syntax colours, and the review
//! of the agent's changes on the git history of that directory.
//!
//! Lines the agent changed since your last review are tinted in its colour,
//! with Keep, Revert and Ask why per file and per change. The head shows
//! who controls the window. A file the agent patches or reads opens by
//! itself unless you have unsaved edits or took the window over.

pub(crate) mod files;
pub(crate) mod highlight;
pub(crate) mod review;

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::time::SystemTime;

use egui::text::LayoutJob;
use egui::{Align2, Color32, CornerRadius, Id, Key, KeyboardShortcut, Modifiers, Rect, Sense, Shape, Stroke, StrokeKind, Ui, pos2, vec2};

use super::{AppTab, ButtonStyle, Layout, button, fit, pill};
use crate::engine::Core;
use crate::theme::{self, c, ca};
use crate::world::ActionEntry;
use files::Loaded;
use highlight::Lang;
use review::{Hunk, Repo, Tint};

const TABS_H: f32 = 32.0;
const STATUS_H: f32 = 26.0;
const TREE_W: f32 = 200.0;
const TREE_ROW: f32 = 24.0;
const GUTTER_W: f32 = 48.0;
const FONT: f32 = 13.0;
const LINE_H: f32 = 20.0;
const PAD_TOP: f32 = 10.0;
const MAX_TABS: usize = 8;
/// Seconds between scans of the tree and the changed files.
const SCAN_EVERY: f64 = 2.0;
/// Seconds between checks of open files on disk.
const CHECK_EVERY: f64 = 1.0;

const BAR_BG: [u8; 3] = [0x0F, 0x12, 0x18];
const LINE: [u8; 3] = [0x1C, 0x20, 0x29];
const DIM: [u8; 3] = [0x8A, 0x92, 0xA3];
const LINE_NO: [u8; 3] = [0x4A, 0x51, 0x60];

/// Editor state of every editor instance that was shown, by agent and
/// instance number. Each instance has its own open files.
#[derive(Default)]
pub struct Editors {
    agents: HashMap<(String, u32), AgentEditor>,
}

impl Editors {
    pub fn new() -> Self {
        Self::default()
    }

    /// Drops the open files and review state of every editor of an agent
    /// that left the space.
    pub fn forget(&mut self, agent: &str) {
        self.agents.retain(|(a, _), _| a != agent);
    }
}

/// Opens `rel`, a path inside the agent's directory, in the agent's editor
/// and opens the editor app when it is closed. Other apps such as Files and
/// Changes use this. With several editors it is the one you used last
/// (`agent::target`).
pub fn open_file(core: &mut Core, agent: &str, rel: &Path) {
    let Some(dir) = core.agent_dir(agent).map(|d| d.path.clone()) else { return };
    let tab = super::agent::target(core, agent, meerora_protocol::App::Editor);
    let key = (agent.to_string(), tab.instance());
    let mut ed = core.ui.editors.agents.remove(&key).filter(|e| e.dir == dir).unwrap_or_else(|| AgentEditor::new(dir));
    ed.open(rel);
    core.ui.editors.agents.insert(key, ed);
    super::agent::open_later(core, tab);
}

/// Opens `rel` as commit `rev` left it, read-only, in a tab of its own
/// titled `name @ <short hash>`, with "Open current" for the file as it is
/// now. Changes uses this, so its "Open in editor" shows that change's version.
pub fn open_revision(core: &mut Core, agent: &str, rel: &Path, rev: &str) {
    let Some(dir) = core.agent_dir(agent).map(|d| d.path.clone()) else { return };
    let tab = super::agent::target(core, agent, meerora_protocol::App::Editor);
    let key = (agent.to_string(), tab.instance());
    let mut ed = core.ui.editors.agents.remove(&key).filter(|e| e.dir == dir).unwrap_or_else(|| AgentEditor::new(dir));
    ed.open_revision(rel, rev);
    core.ui.editors.agents.insert(key, ed);
    super::agent::open_later(core, tab);
}

/// Draws editor `n` of agent `index`. Returns true while the editor has
/// keyboard focus, so Esc stays in the tile. Files the agent patches or
/// reads open in editor 1 only, further editors are yours.
pub fn tile(ui: &mut Ui, core: &mut Core, index: usize, n: u32, head: Rect, body: Rect) -> bool {
    let a = &core.world.agents[index];
    let id = a.id.clone();
    let key = (id.clone(), n);
    let Some(dir) = core.agent_dir(&id).map(|d| d.path.clone()) else { return false };
    let mut ed = core.ui.editors.agents.remove(&key).filter(|e| e.dir == dir).unwrap_or_else(|| AgentEditor::new(dir));
    let info = AgentInfo {
        id: super::instance_key(&id, n),
        name: a.name.clone(),
        color: a.color,
        now: core.world.time,
        run: a.run.as_ref().map(|r| r.id.clone()),
        task: a.run.as_ref().map(|r| r.task.lines().next().unwrap_or_default().to_string()),
        portrait: core.portraits.get(&id),
        selected: core.ui.tab == AppTab::Editor(n),
    };
    let actions: Vec<ActionEntry> =
        if n == 1 { a.actions.iter().filter(|e| ed.seen.is_none_or(|s| e.at > s)).cloned().collect() } else { Vec::new() };
    let out = ed.show(ui, &info, &actions, head, body);
    core.ui.editors.agents.insert(key, ed);
    if out.clicked {
        core.ui.tab = AppTab::Editor(n);
    }
    if let Some(question) = out.ask {
        // Ask why: the question goes into the chat input, ready to send (D12).
        core.ui.drafts.insert(id.clone(), question);
        ui.memory_mut(|m| m.request_focus(Id::new(("chat-input", &id))));
        if core.ui.layout == Layout::Fullscreen {
            core.ui.tab = AppTab::Chat;
        }
    }
    out.focused
}

struct AgentInfo {
    /// The instance key for egui ids (`ui::instance_key`), the agent id for editor 1.
    id: String,
    name: String,
    color: [u8; 3],
    now: f64,
    run: Option<String>,
    task: Option<String>,
    portrait: Option<egui::TextureId>,
    /// The editor is the selected tab, so Ctrl+S saves even without focus.
    selected: bool,
}

#[derive(Default)]
struct Out {
    focused: bool,
    clicked: bool,
    ask: Option<String>,
    /// "Open current" on a file at a revision: the file to open as it is now.
    current: Option<PathBuf>,
}

/// Who controls the window (D12).
#[derive(Clone, Copy, Debug, PartialEq)]
enum Control {
    You,
    TakenOver,
    /// The agent's current run patched files.
    Editing,
    /// The agent opened the file at this time.
    OpenedBy(f64),
}

struct Scan {
    tree: Vec<files::Entry>,
    changed: Option<Vec<PathBuf>>,
    /// Commit subjects of open files with changes, which move when the daemon commits a run.
    subjects: Vec<(PathBuf, Option<String>)>,
}

struct AgentEditor {
    dir: PathBuf,
    repo: Result<Repo, String>,
    tree: Vec<files::Entry>,
    expanded: HashSet<PathBuf>,
    changed: HashSet<PathBuf>,
    scan: Option<Receiver<Scan>>,
    scanned_at: f64,
    checked_at: f64,
    tabs: Vec<Buffer>,
    active: usize,
    /// Set by the tree button. Unset, the tree shows when there is room.
    show_tree: Option<bool>,
    /// Time of the newest action already handled.
    seen: Option<f64>,
    opened_by: Option<f64>,
    /// The run that patched files last.
    patch_run: Option<String>,
    taken_over: bool,
    /// A short note in the status bar: text, whether it is an error, since when.
    message: Option<(String, bool, f64)>,
    /// Scroll the active buffer so this line shows.
    scroll_to: Option<usize>,
}

struct Buffer {
    rel: PathBuf,
    lang: Lang,
    text: String,
    /// The file as last read from or written to disk.
    base: String,
    crlf: bool,
    /// Why the file is read-only.
    read_only: Option<String>,
    /// Why the file is not shown at all.
    unshown: Option<String>,
    stamp: Option<(SystemTime, u64)>,
    disk_changed: bool,
    /// `None` when there is no review for this file, `Some(None)` for a file
    /// that is new since the review.
    reviewed: Option<Option<String>>,
    hunks: Vec<Hunk>,
    tints: Vec<Tint>,
    tints_for: u64,
    subject: Option<String>,
    highlight: (u64, LayoutJob),
    /// Line and column of the text cursor, from 1.
    cursor: Option<(usize, usize)>,
    /// The commit whose version of the file this read-only tab shows.
    rev: Option<String>,
}

impl Buffer {
    fn load(dir: &Path, rel: &Path, repo: Option<&Repo>) -> Buffer {
        let path = dir.join(rel);
        let mut b = Buffer {
            rel: rel.to_path_buf(),
            lang: Lang::of(rel),
            text: String::new(),
            base: String::new(),
            crlf: false,
            read_only: None,
            unshown: None,
            stamp: files::stamp(&path),
            disk_changed: false,
            reviewed: None,
            hunks: Vec::new(),
            tints: Vec::new(),
            tints_for: 0,
            subject: None,
            highlight: (0, LayoutJob::default()),
            cursor: None,
            rev: None,
        };
        match files::load(&path) {
            Loaded::Text { text, crlf, read_only } => {
                b.text = text.clone();
                b.base = text;
                b.crlf = crlf;
                b.read_only = read_only;
                if b.read_only.is_none()
                    && let Some(repo) = repo
                {
                    b.reviewed = repo.reviewed_text(rel).ok().map(|r| r.map(|t| t.replace("\r\n", "\n")));
                }
                b.rediff(repo);
            }
            Loaded::Binary(note) | Loaded::Error(note) => b.unshown = Some(note),
        }
        b
    }

    /// `rel` as commit `rev` left it, from `git show`, read-only. Long files
    /// show their first lines, like big files on disk.
    fn at_revision(dir: &Path, rel: &Path, rev: &str) -> Buffer {
        let mut b = Buffer {
            rel: rel.to_path_buf(),
            lang: Lang::of(rel),
            text: String::new(),
            base: String::new(),
            crlf: false,
            read_only: None,
            unshown: None,
            stamp: None,
            disk_changed: false,
            reviewed: None,
            hunks: Vec::new(),
            tints: Vec::new(),
            tints_for: 0,
            subject: None,
            highlight: (0, LayoutJob::default()),
            cursor: None,
            rev: Some(rev.to_string()),
        };
        let short = short_rev(rev);
        if rev.is_empty() || !rev.chars().all(|c| c.is_ascii_hexdigit()) {
            b.unshown = Some(format!("{rev} is not a commit."));
            return b;
        }
        // `./` keeps the path relative to the agent directory.
        let mut cmd = review::git(dir);
        cmd.args(["show", "--no-textconv", &format!("{rev}:./{}", rel.to_string_lossy())]);
        let bytes = match review::output(cmd, "show") {
            Ok(out) => out.stdout,
            Err(_) => {
                b.unshown = Some(format!("{} is not in {short}. That change may have deleted it.", rel.display()));
                return b;
            }
        };
        if files::is_binary(&bytes) {
            b.unshown =
                Some(format!("This is a binary file of {}. The editor shows text files only.", files::human_size(bytes.len() as u64)));
            return b;
        }
        let Ok(text) = String::from_utf8(bytes) else {
            b.unshown = Some(format!("{} in {short} is not UTF-8 text. The editor shows UTF-8 only.", rel.display()));
            return b;
        };
        b.crlf = text.contains("\r\n");
        let mut text = if b.crlf { text.replace("\r\n", "\n") } else { text };
        let shown: usize = text.split_inclusive('\n').take(files::MAX_SHOW_LINES).map(str::len).sum();
        let cut = shown < text.len();
        text.truncate(shown);
        b.read_only = Some(if cut {
            format!("{} as of {short}, read-only. It shows the first {} lines.", rel.display(), files::MAX_SHOW_LINES)
        } else {
            format!("{} as of {short}, read-only.", rel.display())
        });
        b.base = text.clone();
        b.text = text;
        b
    }

    fn dirty(&self) -> bool {
        self.read_only.is_none() && self.unshown.is_none() && self.text != self.base
    }

    /// The agent's changes between the reviewed text and the file on disk.
    fn rediff(&mut self, repo: Option<&Repo>) {
        self.hunks = match &self.reviewed {
            Some(Some(reviewed)) => review::hunks(reviewed, &self.base),
            Some(None) if !self.base.is_empty() => vec![Hunk { old: 0..0, new: 0..review::lines(&self.base).len() }],
            _ => Vec::new(),
        };
        self.tints_for = 0;
        self.subject = if self.hunks.is_empty() { None } else { repo.and_then(|r| r.change_subject(&self.rel)) };
    }

    fn reload(&mut self, dir: &Path, repo: Option<&Repo>) {
        let reviewed = self.reviewed.take();
        let cursor = self.cursor;
        let highlight = std::mem::take(&mut self.highlight);
        *self = Buffer::load(dir, &self.rel, None);
        if self.unshown.is_none() && self.read_only.is_none() {
            self.reviewed = reviewed;
        }
        self.rediff(repo);
        self.cursor = cursor;
        self.highlight = highlight;
    }

    /// Text for git with the file's own line breaks.
    fn with_breaks(&self, text: &str) -> String {
        if self.crlf { text.replace('\n', "\r\n") } else { text.to_string() }
    }

    fn update_tints(&mut self) {
        let key = hash(&self.text) ^ hash(&self.hunks.len());
        if self.tints_for == key {
            return;
        }
        self.tints_for = key;
        self.tints = if self.text == self.base {
            self.hunks.iter().enumerate().map(|(i, h)| Tint { hunk: i, lines: h.new.clone() }).collect()
        } else {
            review::tints(&self.hunks, &self.base, &self.text)
        };
    }

    /// First tinted line of the buffer, from 0.
    fn first_change(&self) -> Option<usize> {
        self.tints.iter().map(|t| t.lines.start).min()
    }
}

fn hash<T: Hash + ?Sized>(v: &T) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    v.hash(&mut h);
    h.finish()
}

/// Reads the tree, the changed files and the commit subjects of `open` on
/// a thread, so git and the file system never stall the UI.
fn start_scan(dir: PathBuf, repo: Option<Repo>, open: Vec<PathBuf>) -> Receiver<Scan> {
    let (tx, rx) = mpsc::channel();
    let spawned = std::thread::Builder::new().name("editor-scan".into()).spawn(move || {
        let changed = repo.as_ref().and_then(|r| r.changed_files().ok());
        let subjects = open.into_iter().map(|rel| (rel.clone(), repo.as_ref().and_then(|r| r.change_subject(&rel)))).collect();
        let _ = tx.send(Scan { tree: files::tree(&dir), changed, subjects });
    });
    if let Err(e) = spawned {
        tracing::warn!("cannot scan the agent directory: {e}");
    }
    rx
}

/// The file an action summary names, if it is one inside `dir`. Summaries
/// look like `memory/notes.md`, `run.py +5 -1` or `a.txt, b.txt`.
fn resolve(dir: &Path, summary: &str) -> Option<PathBuf> {
    let mut candidates = vec![summary.trim()];
    for part in summary.split(", ") {
        candidates.push(part.trim());
        candidates.push(part.split_whitespace().next().unwrap_or_default());
    }
    candidates.into_iter().filter(|c| !c.is_empty()).find_map(|c| files::inside(dir, c))
}

/// The short form of a commit id, as git prints it.
fn short_rev(rev: &str) -> &str {
    rev.get(..7).unwrap_or(rev)
}

fn ago(seconds: f64) -> String {
    let s = seconds.max(0.0) as u64;
    match s {
        0..60 => format!("{s} S AGO"),
        60..3600 => format!("{} MIN AGO", s / 60),
        _ => format!("{} H AGO", s / 3600),
    }
}

/// `dir` with the home directory as `~`.
fn short_dir(dir: &Path) -> String {
    match std::env::var_os("HOME").map(PathBuf::from) {
        Some(home) if dir.starts_with(&home) => format!("~/{}", dir.strip_prefix(&home).unwrap_or(dir).display()),
        _ => dir.display().to_string(),
    }
}

impl AgentEditor {
    fn new(dir: PathBuf) -> Self {
        let repo = Repo::open(&dir).and_then(|r| r.reviewed().map(|_| r));
        if let Err(e) = &repo {
            tracing::info!("review is off for {}: {e}", dir.display());
        }
        let tree = files::tree(&dir);
        Self {
            dir,
            repo,
            tree,
            expanded: HashSet::new(),
            changed: HashSet::new(),
            scan: None,
            scanned_at: f64::NEG_INFINITY,
            checked_at: 0.0,
            tabs: Vec::new(),
            active: 0,
            show_tree: None,
            seen: None,
            opened_by: None,
            patch_run: None,
            taken_over: false,
            message: None,
            scroll_to: None,
        }
    }

    fn repo(&self) -> Option<&Repo> {
        self.repo.as_ref().ok()
    }

    fn control(&self, info: &AgentInfo) -> Control {
        if self.taken_over {
            Control::TakenOver
        } else if info.run.is_some() && self.patch_run == info.run {
            Control::Editing
        } else if let Some(at) = self.opened_by {
            Control::OpenedBy(at)
        } else {
            Control::You
        }
    }

    fn show(&mut self, ui: &mut Ui, info: &AgentInfo, actions: &[ActionEntry], head: Rect, body: Rect) -> Out {
        self.poll(info.now);
        self.apply_actions(info, actions);
        self.check_disk(info.now);
        let mut out = Out::default();
        let control = self.control(info);
        self.draw_head(ui, info, head, control);

        let tabs_r = Rect::from_min_size(body.min, vec2(body.width(), TABS_H));
        let status_r = Rect::from_min_max(pos2(body.min.x, body.max.y - STATUS_H), body.max);
        let main = Rect::from_min_max(pos2(body.min.x, tabs_r.max.y), pos2(body.max.x, status_r.min.y));
        let show_tree = self.show_tree.unwrap_or(self.tabs.is_empty() || body.width() >= 760.0);
        let (tree_r, code_r) = if show_tree {
            let w = TREE_W.min((body.width() * 0.42).floor());
            (Some(Rect::from_min_size(main.min, vec2(w, main.height()))), Rect::from_min_max(pos2(main.min.x + w, main.min.y), main.max))
        } else {
            (None, main)
        };
        let painter = ui.painter_at(body);
        painter.rect_filled(main, CornerRadius::ZERO, c(theme::PANEL));
        self.draw_tabs(ui, info, tabs_r, show_tree);
        if let Some(r) = tree_r {
            self.draw_tree(ui, info, r);
        }
        self.draw_code(ui, info, code_r, control, &mut out);
        if let Some(rel) = out.current.take() {
            self.open(&rel);
        }
        self.draw_status(ui, info, status_r);
        if matches!(control, Control::Editing | Control::OpenedBy(_)) {
            // The agent controls the window: the frame takes the working colour, as in screen 2.
            let frame = Rect::from_min_max(head.min, pos2(head.max.x, body.max.y + 1.0));
            ui.painter().rect_stroke(frame, CornerRadius::same(14), Stroke::new(1.0, ca(theme::WORKING, 110)), StrokeKind::Inside);
        }

        let save_key = KeyboardShortcut::new(Modifiers::COMMAND, Key::S);
        if (out.focused || info.selected) && ui.input_mut(|i| i.consume_shortcut(&save_key)) {
            self.save(info.now);
        }
        if ui.rect_contains_pointer(body) && ui.input(|i| i.pointer.any_pressed()) {
            out.clicked = true;
        }
        out
    }

    fn poll(&mut self, now: f64) {
        if let Some(rx) = &self.scan {
            match rx.try_recv() {
                Ok(scan) => {
                    self.tree = scan.tree;
                    if let Some(changed) = scan.changed {
                        self.changed = changed.into_iter().collect();
                    }
                    for (rel, subject) in scan.subjects {
                        if let Some(b) = self.tabs.iter_mut().find(|b| b.rel == rel && !b.hunks.is_empty()) {
                            b.subject = subject;
                        }
                    }
                    self.scan = None;
                }
                Err(mpsc::TryRecvError::Disconnected) => self.scan = None,
                Err(mpsc::TryRecvError::Empty) => {}
            }
        } else if now - self.scanned_at > SCAN_EVERY {
            self.scanned_at = now;
            let open = self.tabs.iter().filter(|b| !b.hunks.is_empty()).map(|b| b.rel.clone()).collect();
            self.scan = Some(start_scan(self.dir.clone(), self.repo().cloned(), open));
        }
    }

    /// Opens the files the agent patched or read (D12). The first time only
    /// the newest action counts.
    fn apply_actions(&mut self, info: &AgentInfo, actions: &[ActionEntry]) {
        let first = self.seen.is_none();
        let relevant: Vec<&ActionEntry> = actions.iter().filter(|a| a.tool == "editor.patch" || a.tool == "files.read").collect();
        let todo: Vec<&ActionEntry> = if first { relevant.last().copied().into_iter().collect() } else { relevant };
        for a in todo {
            if a.tool == "editor.patch" && !first {
                self.patch_run = info.run.clone();
                self.checked_at = f64::NEG_INFINITY;
            }
            if self.taken_over {
                continue;
            }
            let Some(rel) = resolve(&self.dir, a.target()) else { continue };
            if self.tabs.get(self.active).is_some_and(|b| b.dirty() && b.rel != rel) {
                continue;
            }
            self.open(&rel);
            self.opened_by = Some(a.at);
            if let Some(b) = self.tabs.get_mut(self.active) {
                b.update_tints();
                self.scroll_to = b.first_change();
            }
        }
        self.seen = Some(actions.last().map_or(self.seen.unwrap_or(f64::NEG_INFINITY), |a| a.at));
    }

    /// Reloads open files that changed on disk, unless they have unsaved edits.
    fn check_disk(&mut self, now: f64) {
        if now - self.checked_at < CHECK_EVERY {
            return;
        }
        self.checked_at = now;
        let repo = self.repo().cloned();
        for b in &mut self.tabs {
            // A file at a revision never changes.
            if b.rev.is_some() {
                continue;
            }
            let stamp = files::stamp(&self.dir.join(&b.rel));
            if stamp == b.stamp {
                continue;
            }
            if b.dirty() {
                b.disk_changed = true;
            } else {
                b.reload(&self.dir, repo.as_ref());
            }
        }
    }

    fn open(&mut self, rel: &Path) {
        if let Some(i) = self.tabs.iter().position(|b| b.rel == rel && b.rev.is_none()) {
            self.active = i;
            return;
        }
        let buf = Buffer::load(&self.dir, rel, self.repo());
        self.tabs.push(buf);
        if self.tabs.len() > MAX_TABS
            && let Some(old) = self.tabs.iter().position(|b| !b.dirty())
        {
            self.tabs.remove(old);
        }
        self.active = self.tabs.iter().position(|b| b.rel == rel && b.rev.is_none()).unwrap_or(0);
        let mut dir = rel.parent();
        while let Some(d) = dir.filter(|d| !d.as_os_str().is_empty()) {
            self.expanded.insert(d.to_path_buf());
            dir = d.parent();
        }
    }

    /// Opens `rel` as commit `rev` left it, in a read-only tab of its own.
    fn open_revision(&mut self, rel: &Path, rev: &str) {
        let at = |b: &Buffer| b.rel == rel && b.rev.as_deref() == Some(rev);
        if let Some(i) = self.tabs.iter().position(at) {
            self.active = i;
            return;
        }
        self.tabs.push(Buffer::at_revision(&self.dir, rel, rev));
        if self.tabs.len() > MAX_TABS
            && let Some(old) = self.tabs.iter().position(|b| !b.dirty())
        {
            self.tabs.remove(old);
        }
        self.active = self.tabs.iter().position(at).unwrap_or(0);
    }

    fn draw_head(&mut self, ui: &mut Ui, info: &AgentInfo, head: Rect, control: Control) {
        let (chip, color) = match control {
            Control::You => ("YOU".to_string(), [0xB9, 0xBF, 0xCC]),
            Control::TakenOver => ("YOU · TAKEN OVER".to_string(), theme::NEEDS_YOU),
            Control::Editing => (format!("{} IS EDITING", info.name.to_uppercase()), theme::WORKING),
            Control::OpenedBy(at) => (format!("OPENED BY {} · {}", info.name.to_uppercase(), ago(info.now - at)), theme::WORKING),
        };
        let mut right = head.max.x - 12.0;
        if control != Control::You {
            let b = Rect::from_min_size(pos2(right - 92.0, head.center().y - 13.0), vec2(92.0, 26.0));
            let text = if control == Control::TakenOver { "Hand back" } else { "Take over" };
            if button(ui, b, text, ButtonStyle::Quiet).clicked() {
                self.taken_over = control != Control::TakenOver;
                if self.taken_over {
                    self.opened_by = None;
                }
            }
            right = b.min.x - 10.0;
        }
        let chip_r = pill(ui.painter(), pos2(right, head.center().y), &chip, color);
        let painter = ui.painter();
        let x = head.min.x + 76.0;
        let dir_text = fit(painter, &short_dir(&self.dir), theme::mono(11.0), chip_r.min.x - x - 12.0);
        painter.text(pos2(x, head.center().y), Align2::LEFT_CENTER, dir_text, theme::mono(11.0), c(DIM));
    }

    fn draw_tabs(&mut self, ui: &mut Ui, info: &AgentInfo, r: Rect, show_tree: bool) {
        let painter = ui.painter_at(r);
        painter.rect_filled(r, CornerRadius::ZERO, c(BAR_BG));
        super::hline(&painter, r.max.y - 0.5, r.min.x, r.max.x);
        let toggle = Rect::from_min_size(r.min, vec2(36.0, r.height()));
        let resp = ui.interact(toggle, Id::new(("editor-tree-toggle", &info.id)), Sense::click()).on_hover_text(if show_tree {
            "Hide the files"
        } else {
            "Show the files"
        });
        let painter = ui.painter_at(r);
        if resp.hovered() || show_tree {
            painter.rect_filled(toggle.shrink(4.0), CornerRadius::same(6), c([0x1A, 0x1F, 0x2A]));
        }
        tree_icon(&painter, toggle.center(), if show_tree { c(theme::TEXT) } else { c(theme::TEXT_2) });
        if resp.clicked() {
            self.show_tree = Some(!show_tree);
        }
        painter.line_segment([pos2(toggle.max.x, r.min.y), pos2(toggle.max.x, r.max.y)], Stroke::new(1.0, c(LINE)));

        let mut x = toggle.max.x;
        let mut activate = None;
        let mut close = None;
        for (i, b) in self.tabs.iter().enumerate() {
            let name = b.rel.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let name = match &b.rev {
                Some(rev) => format!("{name} @ {}", short_rev(rev)),
                None => name,
            };
            let font = theme::mono(12.0);
            let text_w = painter.layout_no_wrap(name.clone(), font.clone(), Color32::WHITE).size().x;
            let marks = b.dirty() || !b.hunks.is_empty();
            let w = text_w + 24.0 + if marks { 12.0 } else { 0.0 } + 14.0;
            if x + w > r.max.x && i != self.active {
                continue;
            }
            let tab = Rect::from_min_size(pos2(x, r.min.y), vec2(w, r.height() - 1.0));
            let resp = ui.interact(tab, Id::new(("editor-tab", &info.id, i)), Sense::click());
            let painter = ui.painter_at(r);
            let active = i == self.active;
            if active {
                painter.rect_filled(tab, CornerRadius::ZERO, c(theme::PANEL));
            } else if resp.hovered() {
                painter.rect_filled(tab, CornerRadius::ZERO, c([0x13, 0x17, 0x1F]));
            }
            painter.text(
                pos2(tab.min.x + 12.0, tab.center().y),
                Align2::LEFT_CENTER,
                &name,
                font,
                if active { c(theme::TEXT) } else { c(DIM) },
            );
            let mut mx = tab.min.x + 12.0 + text_w + 8.0;
            if !b.hunks.is_empty() {
                painter.circle_filled(pos2(mx + 3.0, tab.center().y), 3.0, c(info.color));
                mx += 10.0;
            }
            if b.dirty() {
                painter.circle_stroke(pos2(mx + 3.0, tab.center().y), 3.0, Stroke::new(1.4, c(theme::TEXT)));
            }
            // Close, for tabs without unsaved edits.
            let close_r = Rect::from_center_size(pos2(tab.max.x - 11.0, tab.center().y), vec2(14.0, 14.0));
            if (resp.hovered() || active) && !b.dirty() {
                let close_resp = ui.interact(close_r, Id::new(("editor-tab-close", &info.id, i)), Sense::click());
                let col = if close_resp.hovered() { c(theme::TEXT) } else { c(LINE_NO) };
                let painter = ui.painter_at(r);
                let k = 3.5;
                painter.line_segment([close_r.center() - vec2(k, k), close_r.center() + vec2(k, k)], Stroke::new(1.3, col));
                painter.line_segment([close_r.center() + vec2(-k, k), close_r.center() + vec2(k, -k)], Stroke::new(1.3, col));
                if close_resp.clicked() {
                    close = Some(i);
                }
            }
            if resp.clicked() && close.is_none() {
                activate = Some(i);
            }
            if resp.middle_clicked() && !b.dirty() {
                close = Some(i);
            }
            let painter = ui.painter_at(r);
            painter.line_segment([pos2(tab.max.x, r.min.y), pos2(tab.max.x, r.max.y)], Stroke::new(1.0, c(LINE)));
            x = tab.max.x;
        }
        if let Some(i) = close {
            self.tabs.remove(i);
            if self.active >= i && self.active > 0 {
                self.active -= 1;
            }
        } else if let Some(i) = activate {
            self.active = i;
            self.opened_by = None;
        }
    }

    fn draw_tree(&mut self, ui: &mut Ui, info: &AgentInfo, r: Rect) {
        let painter = ui.painter_at(r);
        painter.rect_filled(r, CornerRadius::ZERO, c(BAR_BG));
        painter.line_segment([pos2(r.max.x - 0.5, r.min.y), pos2(r.max.x - 0.5, r.max.y)], Stroke::new(1.0, c(LINE)));
        let active = self.tabs.get(self.active).map(|b| b.rel.clone());
        let mut toggle = None;
        let mut open = None;
        let mut child =
            ui.new_child(egui::UiBuilder::new().max_rect(r.shrink2(vec2(0.0, 6.0))).layout(egui::Layout::top_down(egui::Align::Min)));
        egui::ScrollArea::vertical().id_salt(("editor-tree", &info.id)).auto_shrink([false, false]).show(&mut child, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            let width = r.width() - 1.0;
            let mut collapsed_below: Option<usize> = None;
            for e in &self.tree {
                if let Some(depth) = collapsed_below {
                    if e.depth > depth {
                        continue;
                    }
                    collapsed_below = None;
                }
                let open_dir = e.dir && self.expanded.contains(&e.rel);
                if e.dir && !open_dir {
                    collapsed_below = Some(e.depth);
                }
                let (row, resp) = ui.allocate_exact_size(vec2(width, TREE_ROW), Sense::click());
                if !ui.is_rect_visible(row) {
                    continue;
                }
                let painter = ui.painter();
                let selected = active.as_deref() == Some(e.rel.as_path());
                if selected {
                    painter.rect_filled(row.shrink2(vec2(6.0, 1.0)), CornerRadius::same(6), c([0x1A, 0x1F, 0x2A]));
                } else if resp.hovered() {
                    painter.rect_filled(row.shrink2(vec2(6.0, 1.0)), CornerRadius::same(6), c([0x15, 0x19, 0x22]));
                }
                let x = row.min.x + 14.0 + e.depth as f32 * 14.0;
                if e.dir {
                    chevron(painter, pos2(x + 3.0, row.center().y), open_dir, c(theme::TEXT_2));
                }
                let name = e.name();
                let color = if selected {
                    c(theme::TEXT)
                } else if name.starts_with('.') {
                    c(LINE_NO)
                } else if e.dir {
                    c([0xB9, 0xBF, 0xCC])
                } else {
                    c(DIM)
                };
                let changed = !e.dir && self.changed.contains(&e.rel);
                let room = row.max.x - x - 14.0 - if changed { 16.0 } else { 6.0 };
                let shown = fit(painter, &name, theme::mono(12.0), room);
                painter.text(pos2(x + 12.0, row.center().y), Align2::LEFT_CENTER, shown, theme::mono(12.0), color);
                if changed {
                    painter.circle_filled(pos2(row.max.x - 16.0, row.center().y), 3.0, c(info.color));
                }
                if resp.clicked() {
                    if e.dir {
                        toggle = Some(e.rel.clone());
                    } else {
                        open = Some(e.rel.clone());
                    }
                }
            }
            if self.tree.is_empty() {
                ui.add_space(8.0);
                ui.label(egui::RichText::new("  The directory is empty").font(theme::ui(13.0)).color(c(theme::TEXT_2)));
            }
        });
        if let Some(d) = toggle
            && !self.expanded.remove(&d)
        {
            self.expanded.insert(d);
        }
        if let Some(rel) = open {
            self.open(&rel);
            self.opened_by = None;
            if let Some(b) = self.tabs.get_mut(self.active) {
                b.update_tints();
                self.scroll_to = b.first_change();
            }
        }
    }

    fn draw_code(&mut self, ui: &mut Ui, info: &AgentInfo, r: Rect, control: Control, out: &mut Out) {
        let Some(b) = self.tabs.get_mut(self.active) else {
            let painter = ui.painter_at(r);
            let text = if self.tree.is_empty() { "Nothing here yet" } else { "Open a file from the tree" };
            // Wrapped, because a dragged tile can be narrower than the line.
            let title = painter.layout(text.to_string(), theme::ui_semibold(15.0), c([0xC9, 0xCE, 0xD9]), (r.width() - 32.0).max(40.0));
            painter.galley(pos2(r.center().x - title.size().x / 2.0, r.center().y - 2.0 - title.size().y), title, c([0xC9, 0xCE, 0xD9]));
            let note = match &self.repo {
                Ok(_) => format!("Changes by {} since your last review show in its colour.", info.name),
                Err(e) => format!("Review is off: {e}."),
            };
            let galley = painter.layout(note, theme::ui(13.0), c(theme::TEXT_2), (r.width() - 48.0).min(380.0));
            painter.galley(pos2(r.center().x - galley.size().x / 2.0, r.center().y + 12.0), galley, c(theme::TEXT_2));
            return;
        };
        if let Some(note) = &b.unshown {
            let painter = ui.painter_at(r);
            painter.text(
                r.center() - vec2(0.0, 12.0),
                Align2::CENTER_CENTER,
                b.rel.display().to_string(),
                theme::mono(12.5),
                c(theme::TEXT),
            );
            let galley = painter.layout(note.clone(), theme::ui(13.5), c(theme::TEXT_2), (r.width() - 48.0).min(380.0));
            painter.galley(pos2(r.center().x - galley.size().x / 2.0, r.center().y + 8.0), galley, c(theme::TEXT_2));
            return;
        }
        b.update_tints();
        let agent_color = info.color;
        let r = match &b.read_only {
            Some(note) => {
                // Big files open read-only, with the note on top.
                let banner = Rect::from_min_size(r.min, vec2(r.width(), 28.0));
                let painter = ui.painter_at(banner);
                painter.rect_filled(banner, CornerRadius::ZERO, ca(theme::WORKING, 16));
                super::hline(&painter, banner.max.y - 0.5, banner.min.x, banner.max.x);
                // A file at a revision offers the file as it is now.
                let mut room = banner.width() - 24.0;
                if b.rev.is_some() {
                    let open = Rect::from_min_size(pos2(banner.max.x - 8.0 - 112.0, banner.center().y - 11.0), vec2(112.0, 22.0));
                    if button(ui, open, "Open current", ButtonStyle::Quiet).on_hover_text("Opens the file as it is now").clicked() {
                        out.current = Some(b.rel.clone());
                    }
                    room -= 124.0;
                }
                let text = fit(&painter, note, theme::ui(12.5), room);
                painter.text(
                    pos2(banner.min.x + 12.0, banner.center().y),
                    Align2::LEFT_CENTER,
                    text,
                    theme::ui(12.5),
                    c([0xE8, 0xD9, 0xBF]),
                );
                Rect::from_min_max(pos2(r.min.x, banner.max.y), r.max)
            }
            None => r,
        };
        let gutter = Rect::from_min_size(r.min, vec2(GUTTER_W, r.height()));
        let text_r = Rect::from_min_max(pos2(gutter.max.x, r.min.y), r.max);
        let edit_id = Id::new(("editor-text", &info.id, &b.rel, &b.rev));
        let focused_before = ui.memory(|m| m.has_focus(edit_id));

        let card_h = if b.hunks.is_empty() { 0.0 } else { card_height(ui, b, info, r.width()) };
        let lang = b.lang;
        let editable = b.read_only.is_none();
        let scroll_to = self.scroll_to.take();
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(text_r).layout(egui::Layout::top_down(egui::Align::Min)));
        child.set_clip_rect(text_r.intersect(ui.clip_rect()));
        let mut area = egui::ScrollArea::both().id_salt(("editor-scroll", &info.id, &b.rel, &b.rev)).auto_shrink([false, false]);
        if let Some(line) = scroll_to {
            // Only scroll when the change is not in view at the top already.
            let y = PAD_TOP + line as f32 * LINE_H;
            let offset = if y + 4.0 * LINE_H < text_r.height() - card_h { 0.0 } else { (y - text_r.height() * 0.3).max(0.0) };
            area = area.vertical_scroll_offset(offset);
        }
        let shown = area.show(&mut child, |ui| {
            ui.add_space(PAD_TOP);
            let bg = ui.painter().add(Shape::Noop);
            let highlight = &mut b.highlight;
            let mut layouter = |ui: &Ui, text: &dyn egui::TextBuffer, _wrap: f32| {
                let s = text.as_str();
                let key = hash(s) ^ hash(&lang);
                if highlight.0 != key {
                    highlight.1 = highlight::layout_job(s, lang, theme::mono(FONT), LINE_H);
                    highlight.0 = key;
                }
                ui.fonts_mut(|f| f.layout_job(highlight.1.clone()))
            };
            let width = (text_r.width() - 16.0).max(40.0);
            let te_out = if editable {
                egui::TextEdit::multiline(&mut b.text)
                    .id(edit_id)
                    .font(theme::mono(FONT))
                    .frame(egui::Frame::NONE)
                    .margin(egui::Margin { left: 6, right: 24, top: 0, bottom: 0 })
                    .desired_width(width)
                    .desired_rows(1)
                    .lock_focus(true)
                    .layouter(&mut layouter)
                    .show(ui)
            } else {
                egui::TextEdit::multiline(&mut b.text.as_str())
                    .id(edit_id)
                    .font(theme::mono(FONT))
                    .frame(egui::Frame::NONE)
                    .margin(egui::Margin { left: 6, right: 24, top: 0, bottom: 0 })
                    .desired_width(width)
                    .desired_rows(1)
                    .layouter(&mut layouter)
                    .show(ui)
            };
            ui.add_space(card_h + 24.0);
            // Tints behind the text, across the whole visible width (D12).
            let clip = ui.clip_rect();
            let rows = &te_out.galley.rows;
            let mut shapes = Vec::new();
            for t in &b.tints {
                let Some((y0, y1)) = rows_span(rows, te_out.galley_pos.y, &t.lines) else { continue };
                if t.lines.is_empty() {
                    shapes.push(Shape::line_segment([pos2(clip.min.x, y0), pos2(clip.max.x, y0)], Stroke::new(2.0, ca(agent_color, 160))));
                } else {
                    shapes.push(Shape::rect_filled(Rect::from_x_y_ranges(clip.min.x..=clip.max.x, y0..=y1), 0.0, ca(agent_color, 26)));
                }
            }
            ui.painter().set(bg, Shape::Vec(shapes));
            (te_out.galley.clone(), te_out.galley_pos, te_out.response.response.changed(), te_out.cursor_range)
        });
        let (galley, galley_pos, changed, cursor) = shown.inner;
        let viewport = shown.inner_rect;
        let focused = ui.memory(|m| m.has_focus(edit_id));
        out.focused = focused || focused_before;
        if changed {
            // Typing yourself while the agent edits takes over, as in the terminal.
            if control == Control::Editing {
                self.taken_over = true;
            }
            self.opened_by = None;
        }
        if let Some(range) = cursor {
            let index = range.primary.index.0;
            let before: String = b.text.chars().take(index).collect();
            let line = before.matches('\n').count() + 1;
            let col = before.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
            b.cursor = Some((line, col));
        }

        // Gutter with line numbers and the agent's bar on tinted lines.
        let painter = ui.painter_at(gutter);
        painter.rect_filled(gutter, CornerRadius::ZERO, c(theme::PANEL));
        let rows = &galley.rows;
        for t in &b.tints {
            let Some((y0, y1)) = rows_span(rows, galley_pos.y, &t.lines) else { continue };
            if t.lines.is_empty() {
                painter.line_segment([pos2(gutter.min.x, y0), pos2(gutter.max.x, y0)], Stroke::new(2.0, ca(agent_color, 160)));
                painter.add(Shape::convex_polygon(
                    vec![pos2(gutter.min.x, y0 - 4.0), pos2(gutter.min.x + 5.0, y0), pos2(gutter.min.x, y0 + 4.0)],
                    c(agent_color),
                    Stroke::NONE,
                ));
            } else {
                painter.rect_filled(Rect::from_x_y_ranges(gutter.min.x..=gutter.max.x, y0..=y1), 0.0, ca(agent_color, 26));
                painter.rect_filled(Rect::from_x_y_ranges(gutter.min.x..=gutter.min.x + 3.0, y0..=y1), 0.0, c(agent_color));
            }
        }
        let tinted = |line: usize| b.tints.iter().any(|t| t.lines.contains(&line));
        let cursor_line = b.cursor.map(|(l, _)| l - 1).filter(|_| focused);
        for (i, row) in rows.iter().enumerate() {
            let y = galley_pos.y + row.pos.y;
            if y > gutter.max.y || y + LINE_H < gutter.min.y {
                continue;
            }
            let color = if cursor_line == Some(i) {
                c(theme::TEXT)
            } else if tinted(i) {
                ca(agent_color, 220)
            } else {
                c(LINE_NO)
            };
            painter.text(pos2(gutter.max.x - 12.0, y + LINE_H / 2.0), Align2::RIGHT_CENTER, format!("{}", i + 1), theme::mono(12.0), color);
        }

        // Per change: Keep, Revert and Ask why on the first line of the change, while the pointer is on it.
        let pointer = ui.ctx().pointer_hover_pos().filter(|p| r.contains(*p));
        let mut action: Option<(usize, HunkAction)> = None;
        let mut seen_hunks = HashSet::new();
        for t in &b.tints {
            if !seen_hunks.insert(t.hunk) {
                continue;
            }
            let Some((y0, y1)) = rows_span(rows, galley_pos.y, &t.lines) else { continue };
            let hot = pointer.is_some_and(|p| p.y >= y0 - 2.0 && p.y <= y1.max(y0 + LINE_H) + 2.0)
                || cursor_line.is_some_and(|l| t.lines.contains(&l));
            if !hot || y0 < viewport.min.y - 4.0 || y0 > viewport.max.y - 22.0 {
                continue;
            }
            let dirty = b.dirty();
            let mut x = viewport.max.x - 8.0;
            let y = if t.lines.is_empty() { y0 + 3.0 } else { y0 + 1.0 };
            for (label, what) in [("Ask why", HunkAction::Ask), ("Revert", HunkAction::Revert), ("Keep", HunkAction::Keep)] {
                let enabled = match what {
                    HunkAction::Revert => !dirty && matches!(b.reviewed, Some(Some(_))),
                    _ => true,
                };
                let font = theme::mono(10.5);
                let w = ui.painter().layout_no_wrap(label.to_string(), font.clone(), Color32::WHITE).size().x + 14.0;
                let rect = Rect::from_min_size(pos2(x - w, y), vec2(w, 18.0));
                x = rect.min.x - 4.0;
                let resp = ui.interact(
                    rect,
                    Id::new(("editor-hunk", &info.id, t.hunk, label)),
                    if enabled { Sense::click() } else { Sense::hover() },
                );
                let painter = ui.painter_at(text_r);
                let hovered = resp.hovered() && enabled;
                let fill = if what == HunkAction::Keep { ca(agent_color, if hovered { 255 } else { 220 }) } else { c([0x1A, 0x17, 0x10]) };
                let text_color = if what == HunkAction::Keep {
                    c([0x1C, 0x14, 0x08])
                } else if enabled {
                    c([0xE8, 0xDC, 0xC6])
                } else {
                    c(LINE_NO)
                };
                painter.rect(rect, CornerRadius::same(5), fill, Stroke::new(1.0, ca(agent_color, 90)), StrokeKind::Inside);
                painter.text(rect.center(), Align2::CENTER_CENTER, label, font, text_color);
                if hovered {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                }
                if !enabled {
                    resp.on_hover_text("Save or discard your edits first");
                } else if resp.clicked() {
                    action = Some((t.hunk, what));
                }
            }
        }

        // The agent's labelled cursor at its newest change while it controls the window (D12).
        if matches!(control, Control::Editing | Control::OpenedBy(_))
            && let Some(t) = b.tints.iter().rev().find(|t| !t.lines.is_empty())
            && let Some(row) = rows.get(t.lines.end - 1)
        {
            let x = galley_pos.x + row.rect().max.x + 7.0;
            let y = galley_pos.y + row.pos.y;
            if viewport.contains(pos2(x.min(viewport.max.x - 1.0), y + 2.0)) {
                let painter = ui.painter_at(text_r);
                painter.rect_filled(Rect::from_min_size(pos2(x, y + 1.0), vec2(2.0, LINE_H - 2.0)), 0.0, c(agent_color));
                let font = theme::mono(10.0);
                let galley = painter.layout_no_wrap(info.name.clone(), font, c([0x1C, 0x14, 0x08]));
                let size = galley.size() + vec2(10.0, 2.0);
                let chip = Rect::from_min_size(pos2(x.min(viewport.max.x - size.x - 4.0), y - 14.0), size);
                painter.rect_filled(chip, CornerRadius { nw: 4, ne: 4, se: 4, sw: 0 }, c(agent_color));
                painter.galley(chip.min + vec2(5.0, 1.0), galley, c([0x1C, 0x14, 0x08]));
            }
        }

        if !b.hunks.is_empty() {
            let card = Rect::from_min_max(pos2(r.min.x + GUTTER_W, r.max.y - 14.0 - card_h), pos2(r.max.x - 14.0, r.max.y - 14.0));
            if let Some(what) = draw_card(ui, b, info, card) {
                action = Some((usize::MAX, what));
            }
        }
        if let Some((hunk, what)) = action {
            self.review_action(info, hunk, what, out);
        }
    }

    /// Keep, Revert or Ask why for one change, or for the whole file when
    /// `hunk` is `usize::MAX`.
    fn review_action(&mut self, info: &AgentInfo, hunk: usize, what: HunkAction, out: &mut Out) {
        let repo = self.repo().cloned();
        let Some(b) = self.tabs.get_mut(self.active) else { return };
        let path = self.dir.join(&b.rel);
        let all = hunk == usize::MAX;
        let result: Result<(), String> = match what {
            HunkAction::Ask => {
                let line = if all { b.first_change() } else { b.tints.iter().find(|t| t.hunk == hunk).map(|t| t.lines.start) };
                out.ask = Some(format!("Why did you change {} line {}?", b.rel.display(), line.unwrap_or(0) + 1));
                Ok(())
            }
            HunkAction::Keep => {
                let Some(repo) = repo.as_ref() else { return };
                let reviewed = match (&b.reviewed, all) {
                    (_, true) => b.base.clone(),
                    (Some(Some(r)), false) => b.hunks.get(hunk).map_or_else(|| r.clone(), |h| review::keep_hunk(r, &b.base, h)),
                    (Some(None), false) => b.base.clone(),
                    (None, false) => return,
                };
                let message = format!("meerora: keep {}'s change to {}", info.name, b.rel.display());
                repo.set_reviewed(&b.rel, &b.with_breaks(&reviewed), &message).map(|()| {
                    b.reviewed = Some(Some(reviewed));
                })
            }
            HunkAction::Revert => {
                let Some(Some(reviewed)) = b.reviewed.clone() else { return };
                if b.dirty() {
                    return;
                }
                let text =
                    if all { reviewed } else { b.hunks.get(hunk).map_or(b.base.clone(), |h| review::revert_hunk(&reviewed, &b.base, h)) };
                files::save(&path, &text, b.crlf).map_err(|e| format!("Cannot write {} ({e}).", b.rel.display())).map(|()| {
                    b.text = text.clone();
                    b.base = text;
                    b.stamp = files::stamp(&path);
                })
            }
        };
        match result {
            Ok(()) => {
                b.rediff(repo.as_ref());
                if b.hunks.is_empty() {
                    self.changed.remove(&b.rel);
                }
                self.message = match what {
                    HunkAction::Keep => Some((format!("Kept {}'s change", info.name), false, info.now)),
                    HunkAction::Revert => Some(("Reverted to your reviewed version".into(), false, info.now)),
                    HunkAction::Ask => None,
                };
            }
            Err(e) => self.message = Some((e, true, info.now)),
        }
    }

    fn save(&mut self, now: f64) {
        let repo = self.repo().cloned();
        let Some(b) = self.tabs.get_mut(self.active) else { return };
        if !b.dirty() {
            return;
        }
        let path = self.dir.join(&b.rel);
        if let Err(e) = files::save(&path, &b.text, b.crlf) {
            self.message = Some((format!("Cannot save {} ({e}).", b.rel.display()), true, now));
            return;
        }
        // Your own edit joins the reviewed state, so it never shows as the agent's.
        if let (Some(repo), Some(Some(reviewed))) = (repo.as_ref(), &b.reviewed) {
            let folded = review::fold_edit(reviewed, &b.base, &b.text);
            if &folded != reviewed {
                match repo.set_reviewed(&b.rel, &b.with_breaks(&folded), &format!("meerora: your edit to {}", b.rel.display())) {
                    Ok(()) => b.reviewed = Some(Some(folded)),
                    Err(e) => tracing::warn!("cannot record your edit as reviewed: {e}"),
                }
            }
        }
        b.base = b.text.clone();
        b.stamp = files::stamp(&path);
        b.disk_changed = false;
        b.rediff(repo.as_ref());
        self.message = Some((format!("Saved {}", b.rel.display()), false, now));
    }

    fn draw_status(&mut self, ui: &mut Ui, info: &AgentInfo, r: Rect) {
        let painter = ui.painter_at(r);
        painter.rect_filled(r, CornerRadius::ZERO, c(BAR_BG));
        super::hline(&painter, r.min.y + 0.5, r.min.x, r.max.x);
        let font = theme::mono(11.0);
        let y = r.center().y;
        let mut x = r.min.x + 12.0;
        let Some(b) = self.tabs.get(self.active) else {
            if let Err(e) = &self.repo {
                let text = fit(&painter, &format!("Review off: {e}"), font.clone(), r.width() - 24.0);
                painter.text(pos2(x, y), Align2::LEFT_CENTER, text, font, c(DIM));
            }
            return;
        };
        let put = |x: &mut f32, text: &str, color: Color32| {
            let rect = painter.text(pos2(*x, y), Align2::LEFT_CENTER, text, font.clone(), color);
            *x = rect.max.x + 14.0;
            rect
        };
        put(&mut x, b.lang.name(), c(DIM));
        if !b.hunks.is_empty() {
            let n = b.hunks.len();
            put(&mut x, &format!("{n} change{} by {}", if n == 1 { "" } else { "s" }, info.name), c(info.color));
        }
        if b.read_only.is_some() {
            put(&mut x, "Read-only", c(DIM));
        }
        if b.dirty() {
            put(&mut x, "Unsaved, Ctrl+S saves", c(theme::TEXT));
        }
        let mut reload = None;
        if b.disk_changed {
            let rect = put(&mut x, "Changed on disk", c(theme::WORKING));
            let link = Rect::from_min_size(pos2(rect.max.x + 2.0, r.min.y + 3.0), vec2(56.0, r.height() - 6.0));
            let resp = ui.interact(link, Id::new(("editor-reload", &info.id)), Sense::click());
            painter.text(link.left_center() + vec2(4.0, 0.0), Align2::LEFT_CENTER, "Reload", font.clone(), c(theme::NEEDS_YOU));
            if resp.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
            reload = resp.clicked().then_some(());
            x = link.max.x + 10.0;
        }
        if self.repo.is_err() {
            let resp_rect = put(&mut x, "Review off", c(DIM));
            if let Err(e) = &self.repo {
                ui.interact(resp_rect, Id::new(("editor-review-off", &info.id)), Sense::hover()).on_hover_text(e.clone());
            }
        }
        if let Some(b) = self.tabs.get(self.active)
            && let Some((line, col)) = b.cursor
        {
            painter.text(pos2(r.max.x - 12.0, y), Align2::RIGHT_CENTER, format!("Ln {line}, Col {col}"), font.clone(), c(DIM));
        }
        if let Some((text, error, at)) = &self.message
            && info.now - at < 6.0
        {
            let room = r.max.x - 110.0 - x;
            if room > 60.0 {
                let shown = fit(&painter, text, font.clone(), room);
                painter.text(pos2(x, y), Align2::LEFT_CENTER, shown, font, if *error { c([0xE0, 0x67, 0x5A]) } else { c(theme::TEXT_2) });
            }
        }
        if reload.is_some() {
            let repo = self.repo().cloned();
            if let Some(b) = self.tabs.get_mut(self.active) {
                b.reload(&self.dir, repo.as_ref());
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum HunkAction {
    Keep,
    Revert,
    Ask,
}

/// Top and bottom of the rows `lines` in screen space. For an empty range
/// both are the top of the row where the removed lines were.
fn rows_span(rows: &[egui::epaint::text::PlacedRow], top: f32, lines: &std::ops::Range<usize>) -> Option<(f32, f32)> {
    if lines.is_empty() {
        let y = match rows.get(lines.start) {
            Some(row) => top + row.pos.y,
            None => top + rows.last().map_or(0.0, |r| r.rect().max.y),
        };
        return Some((y, y));
    }
    let first = rows.get(lines.start)?;
    let last = rows.get(lines.end - 1).unwrap_or(first);
    Some((top + first.pos.y, top + last.pos.y + LINE_H))
}

fn card_text(b: &Buffer, info: &AgentInfo) -> String {
    match (&b.reviewed, &b.subject) {
        (Some(None), _) => format!("{} created this file since your last review.", info.name),
        (_, Some(subject)) => match subject.strip_prefix("run ").and_then(|s| s.split_once(": ")) {
            // The daemon's commits read "run <date> <time>: <task>".
            Some((when, task)) => format!("From the run of {when}: {task}"),
            None => format!("Changed since your last review: {subject}"),
        },
        (_, None) if info.run.is_some() => {
            format!("{} is changing this file in its current run: {}", info.name, info.task.clone().unwrap_or_default())
        }
        _ => "Changed since your last review, not committed yet.".to_string(),
    }
}

fn card_height(ui: &Ui, b: &Buffer, info: &AgentInfo, width: f32) -> f32 {
    let w = (width - GUTTER_W - 14.0 - 28.0).max(80.0);
    let galley = ui.painter().layout(card_text(b, info), theme::ui(13.0), Color32::WHITE, w);
    12.0 + 20.0 + 6.0 + galley.size().y + 10.0 + 32.0 + 12.0
}

/// The review card at the bottom of the editor (screen 2): who changed
/// which lines, why, and Keep, Revert and Ask why for the whole file.
fn draw_card(ui: &mut Ui, b: &Buffer, info: &AgentInfo, card: Rect) -> Option<HunkAction> {
    let painter = ui.painter_at(card.expand(2.0));
    painter.rect(card, CornerRadius::same(12), c([0x1A, 0x17, 0x10]), Stroke::new(1.0, c([0x4A, 0x3C, 0x24])), StrokeKind::Inside);
    // The card hides the text under it from the pointer.
    ui.interact(card, Id::new(("editor-card", &info.id)), Sense::click());
    let mut x = card.min.x + 14.0;
    let row_y = card.min.y + 12.0 + 10.0;
    match info.portrait {
        Some(tex) => {
            let p = Rect::from_center_size(pos2(x + 9.0, row_y), vec2(22.0, 22.0));
            painter.image(tex, p, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
        }
        None => {
            painter.circle_filled(pos2(x + 8.0, row_y), 7.0, c(info.color));
        }
    }
    x += 26.0;
    let name = painter.text(pos2(x, row_y), Align2::LEFT_CENTER, &info.name, theme::ui_bold(13.5), c(theme::TEXT));
    let first = b.tints.iter().map(|t| t.lines.start).min().unwrap_or(0) + 1;
    let last = b.tints.iter().map(|t| t.lines.end.max(t.lines.start + 1)).max().unwrap_or(first);
    let n = b.hunks.len();
    let lines = if last <= first { format!("LINE {first}") } else { format!("LINES {first}-{last}") };
    let meta = format!("{lines} · {n} CHANGE{}", if n == 1 { "" } else { "S" });
    let meta = fit(&painter, &meta, theme::mono(10.0), card.max.x - name.max.x - 24.0);
    painter.text(pos2(name.max.x + 8.0, row_y), Align2::LEFT_CENTER, meta, theme::mono(10.0), c(theme::TEXT_2));

    let text_w = (card.width() - 28.0).max(80.0);
    let galley = painter.layout(card_text(b, info), theme::ui(13.0), c([0xDD, 0xD6, 0xC8]), text_w);
    let text_y = card.min.y + 12.0 + 20.0 + 6.0;
    let text_h = galley.size().y;
    painter.galley(pos2(card.min.x + 14.0, text_y), galley, c([0xDD, 0xD6, 0xC8]));

    let mut bx = card.min.x + 14.0;
    let by = text_y + text_h + 10.0;
    let mut chosen = None;
    let dirty = b.dirty();
    for (label, what) in [("Keep", HunkAction::Keep), ("Revert", HunkAction::Revert), ("Ask why", HunkAction::Ask)] {
        let w = painter.layout_no_wrap(label.to_string(), theme::ui(13.0), Color32::WHITE).size().x + 26.0;
        let rect = Rect::from_min_size(pos2(bx, by), vec2(w, 32.0));
        bx = rect.max.x + 6.0;
        let enabled = match what {
            HunkAction::Revert => !dirty && matches!(b.reviewed, Some(Some(_))),
            _ => true,
        };
        let resp =
            ui.interact(rect, Id::new(("editor-card-button", &info.id, label)), if enabled { Sense::click() } else { Sense::hover() });
        let hovered = resp.hovered() && enabled;
        let painter = ui.painter_at(card.expand(2.0));
        if what == HunkAction::Keep {
            painter.rect_filled(rect, CornerRadius::same(8), if hovered { c([0xF6, 0xC4, 0x78]) } else { c(theme::WORKING) });
            painter.text(rect.center(), Align2::CENTER_CENTER, label, theme::ui_bold(13.0), c([0x1C, 0x14, 0x08]));
        } else {
            let fill = if hovered { c([0x24, 0x1F, 0x15]) } else { Color32::TRANSPARENT };
            painter.rect(rect, CornerRadius::same(8), fill, Stroke::new(1.0, c([0x4A, 0x3C, 0x24])), StrokeKind::Inside);
            let color = if enabled { c([0xE8, 0xDC, 0xC6]) } else { c(LINE_NO) };
            painter.text(rect.center(), Align2::CENTER_CENTER, label, theme::ui(13.0), color);
        }
        if hovered {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        if !enabled {
            let why = if dirty { "Save or discard your edits first" } else { "The file is new. Delete it yourself if you do not want it." };
            resp.on_hover_text(why);
        } else if resp.clicked() {
            chosen = Some(what);
        }
    }
    chosen
}

fn tree_icon(painter: &egui::Painter, center: egui::Pos2, color: Color32) {
    let s = Stroke::new(1.5, color);
    for (dy, x0) in [(-5.0, -6.0), (0.0, -2.0), (5.0, -2.0)] {
        painter.line_segment([center + vec2(x0, dy), center + vec2(6.0, dy)], s);
    }
    painter.line_segment([center + vec2(-6.0, -5.0), center + vec2(-6.0, 5.0)], s);
    painter.line_segment([center + vec2(-6.0, 0.0), center + vec2(-3.0, 0.0)], s);
    painter.line_segment([center + vec2(-6.0, 5.0), center + vec2(-3.0, 5.0)], s);
}

fn chevron(painter: &egui::Painter, center: egui::Pos2, open: bool, color: Color32) {
    let s = Stroke::new(1.4, color);
    if open {
        painter.line_segment([center + vec2(-3.5, -1.5), center + vec2(0.0, 2.0)], s);
        painter.line_segment([center + vec2(0.0, 2.0), center + vec2(3.5, -1.5)], s);
    } else {
        painter.line_segment([center + vec2(-1.5, -3.5), center + vec2(2.0, 0.0)], s);
        painter.line_segment([center + vec2(2.0, 0.0), center + vec2(-1.5, 3.5)], s);
    }
}

// Agent and app standard (D30, agent_apps.rs).

/// What editor `n` of an agent shows, for `app_read`.
pub(crate) struct AgentView {
    /// The open files relative to the agent directory, and whether each has unsaved edits.
    pub open: Vec<(PathBuf, bool)>,
    pub active: Option<ActiveFile>,
}

/// The file that shows in an editor.
pub(crate) struct ActiveFile {
    pub rel: PathBuf,
    pub text: String,
    /// Line and column of the cursor, from 1.
    pub cursor: Option<(usize, usize)>,
    pub unsaved: bool,
    /// Why it is read-only or not shown.
    pub note: Option<String>,
}

/// Editor `n` of `agent`, made when it was never shown.
fn agent_editor<'a>(core: &'a mut Core, agent: &str, n: u32) -> Option<&'a mut AgentEditor> {
    let dir = core.agent_dir(agent)?.path.clone();
    let ed = core.ui.editors.agents.entry((agent.to_string(), n)).or_insert_with(|| AgentEditor::new(dir.clone()));
    if ed.dir != dir {
        *ed = AgentEditor::new(dir);
    }
    Some(ed)
}

pub(crate) fn agent_view(core: &mut Core, agent: &str, n: u32) -> Option<AgentView> {
    let ed = agent_editor(core, agent, n)?;
    let open = ed.tabs.iter().map(|b| (b.rel.clone(), b.dirty())).collect();
    let active = ed.tabs.get(ed.active).map(|b| ActiveFile {
        rel: b.rel.clone(),
        text: b.text.clone(),
        cursor: b.cursor,
        unsaved: b.dirty(),
        note: b.read_only.clone().or_else(|| b.unshown.clone()),
    });
    Some(AgentView { open, active })
}

/// Opens `rel` in editor `n` of `agent` for the agent. The head says the
/// agent opened it, with Take over (D12).
pub(crate) fn agent_open(core: &mut Core, agent: &str, n: u32, rel: &Path) -> bool {
    let now = core.world.time;
    let Some(ed) = agent_editor(core, agent, n) else { return false };
    ed.open(rel);
    ed.opened_by = Some(now);
    if let Some(b) = ed.tabs.get_mut(ed.active) {
        b.update_tints();
        ed.scroll_to = b.first_change();
    }
    true
}

/// Shows line `line` (from 1) of the open file of editor `n` of `agent`.
pub(crate) fn agent_goto(core: &mut Core, agent: &str, n: u32, line: usize) -> Result<PathBuf, String> {
    let now = core.world.time;
    let ed = agent_editor(core, agent, n).ok_or("This agent has no directory.")?;
    let b = ed.tabs.get_mut(ed.active).ok_or(format!("editor:{n} has no open file. Open one with open {{path}} first."))?;
    let lines = b.text.lines().count().max(1);
    if line == 0 || line > lines {
        return Err(format!("{} has {lines} lines, so there is no line {line}.", b.rel.display()));
    }
    b.cursor = Some((line, 1));
    let rel = b.rel.clone();
    ed.scroll_to = Some(line - 1);
    ed.opened_by = Some(now);
    Ok(rel)
}

/// Whether you took editor `n` of `agent` over (D12).
pub(crate) fn taken_over(core: &Core, agent: &str, n: u32) -> bool {
    core.ui.editors.agents.get(&(agent.to_string(), n)).is_some_and(|e| e.taken_over)
}

/// Takes editor `n` of `agent` over, as the button on its head does.
#[cfg(test)]
pub(crate) fn tests_take_over(core: &mut Core, agent: &str, n: u32) {
    if let Some(ed) = agent_editor(core, agent, n) {
        ed.taken_over = true;
    }
}

#[cfg(test)]
mod tests {
    use super::review::tests::TempDir;
    use super::*;

    #[test]
    fn action_summaries_name_files() {
        let tmp = TempDir::new("editor-resolve");
        std::fs::create_dir_all(tmp.0.join("memory")).unwrap();
        std::fs::write(tmp.0.join("run.py"), "x").unwrap();
        std::fs::write(tmp.0.join("memory/notes.md"), "x").unwrap();
        assert_eq!(resolve(&tmp.0, "run.py +5 -1"), Some(PathBuf::from("run.py")));
        assert_eq!(resolve(&tmp.0, "memory/notes.md"), Some(PathBuf::from("memory/notes.md")));
        assert_eq!(resolve(&tmp.0, "missing.txt, memory/notes.md"), Some(PathBuf::from("memory/notes.md")));
        assert_eq!(resolve(&tmp.0, &tmp.0.join("run.py").to_string_lossy()), Some(PathBuf::from("run.py")));
        assert_eq!(resolve(&tmp.0, "TODO in src"), None);
        assert_eq!(resolve(&tmp.0, "/etc/passwd"), None);
        assert_eq!(ago(12.4), "12 S AGO");
        assert_eq!(ago(130.0), "2 MIN AGO");
    }

    fn info(now: f64, run: Option<&str>) -> AgentInfo {
        AgentInfo {
            id: "ember".into(),
            name: "Ember".into(),
            color: [0xC7, 0x78, 0x4A],
            now,
            run: run.map(str::to_string),
            task: run.map(|_| "Fix the timeouts".to_string()),
            portrait: None,
            selected: true,
        }
    }

    /// The agent's run changed run.py since the review started.
    fn reviewed_and_changed(name: &str) -> (TempDir, AgentEditor) {
        let repo = review::tests::agent_repo(name);
        let ed = AgentEditor::new(repo.0.clone());
        assert!(ed.repo.is_ok(), "{:?}", ed.repo.as_ref().err());
        std::fs::write(repo.0.join("run.py"), "import time\n\ndef run():\n    time.sleep(1)\n    return 2\n").unwrap();
        review::tests::run(&repo.0, &["commit", "-qam", "run 2026-10-14 19:41: Sleep first"]);
        (repo, ed)
    }

    fn patch(at: f64, summary: &str) -> ActionEntry {
        ActionEntry { at, tool: "editor.patch".into(), summary: summary.into(), target: None }
    }

    #[test]
    fn a_patch_opens_the_file_unless_you_have_unsaved_edits() {
        let (repo, mut ed) = reviewed_and_changed("editor-open");
        std::fs::write(repo.0.join("notes.md"), "notes\n").unwrap();
        ed.apply_actions(&info(1.0, None), &[patch(1.0, "run.py +2 -1")]);
        assert_eq!(ed.tabs.len(), 1);
        assert_eq!(ed.opened_by, Some(1.0));
        assert_eq!(ed.control(&info(5.0, None)), Control::OpenedBy(1.0));
        assert_eq!(ed.tabs[0].hunks, vec![Hunk { old: 3..4, new: 3..5 }]);
        assert_eq!(ed.tabs[0].subject.as_deref(), Some("run 2026-10-14 19:41: Sleep first"));

        // A patch during the run that is going on: the agent is editing.
        ed.apply_actions(&info(2.0, Some("r1")), &[patch(2.0, "run.py")]);
        assert_eq!(ed.control(&info(3.0, Some("r1"))), Control::Editing);
        assert_eq!(ed.control(&info(3.0, None)), Control::OpenedBy(2.0), "the run ended");

        // You type into run.py, so another patched file does not take the window.
        ed.tabs[0].text.push_str("# mine\n");
        ed.apply_actions(&info(4.0, Some("r1")), &[patch(4.0, "notes.md")]);
        assert_eq!(ed.tabs.len(), 1);
        assert_eq!(ed.tabs[ed.active].rel, PathBuf::from("run.py"));

        // Taken over: the agent's actions are ignored until you hand back.
        ed.tabs[0].text = ed.tabs[0].base.clone();
        ed.taken_over = true;
        ed.apply_actions(&info(5.0, Some("r1")), &[patch(5.0, "notes.md")]);
        assert_eq!(ed.tabs.len(), 1);
    }

    #[test]
    fn a_patch_opens_the_file_of_its_full_target() {
        let (repo, mut ed) = reviewed_and_changed("editor-target");
        let deep = format!("{}/notes.md", "nested-folder".repeat(8));
        std::fs::create_dir_all(repo.0.join(&deep).parent().unwrap()).unwrap();
        std::fs::write(repo.0.join(&deep), "notes\n").unwrap();
        let cut: String = deep.chars().take(79).chain(['…']).collect();
        let action = ActionEntry { at: 1.0, tool: "editor.patch".into(), summary: cut, target: Some(deep.clone()) };
        ed.apply_actions(&info(1.0, None), &[action]);
        assert_eq!(ed.tabs.get(ed.active).map(|b| b.rel.clone()), Some(PathBuf::from(&deep)), "not the cut summary");
    }

    #[test]
    fn a_file_opens_read_only_at_a_revision_beside_the_current_one() {
        let (repo, mut ed) = reviewed_and_changed("editor-revision");
        let first = review::tests::run(&repo.0, &["rev-parse", "HEAD~1"]);
        let old = "import time\n\ndef run():\n    return 1\n";
        ed.open_revision(Path::new("run.py"), &first);
        let b = &ed.tabs[ed.active];
        assert_eq!((b.text.as_str(), b.rev.as_deref()), (old, Some(first.as_str())), "the version of that commit");
        assert!(b.read_only.as_deref().is_some_and(|n| n.contains(short_rev(&first))), "{:?}", b.read_only);
        assert!(!b.dirty() && b.hunks.is_empty(), "no review on an old version");
        assert_eq!(short_rev(&first).len(), 7);

        // The file on disk is newer, and the tab stays at its revision.
        ed.check_disk(10.0);
        assert_eq!(ed.tabs[ed.active].text, old);
        ed.open_revision(Path::new("run.py"), &first);
        assert_eq!(ed.tabs.len(), 1, "the same tab again");
        // "Open current" opens the file as it is now in a tab of its own.
        ed.open(Path::new("run.py"));
        assert_eq!(ed.tabs.len(), 2);
        assert!(ed.tabs[ed.active].rev.is_none() && ed.tabs[ed.active].text.contains("time.sleep(1)"));

        ed.open_revision(Path::new("gone.txt"), &first);
        assert!(ed.tabs[ed.active].unshown.as_deref().is_some_and(|n| n.contains("is not in")), "a file the commit lacks says so");
        ed.open_revision(Path::new("run.py"), "--output=/tmp/x");
        assert!(ed.tabs[ed.active].unshown.as_deref().is_some_and(|n| n.contains("is not a commit")), "no option reaches git");
    }

    #[test]
    fn keep_and_revert_from_the_editor() {
        let (repo, mut ed) = reviewed_and_changed("editor-keep");
        let start = review::tests::run(&repo.0, &["rev-parse", review::REVIEWED_REF]);
        ed.open(Path::new("run.py"));
        let mut out = Out::default();
        // Revert the change, then the file is the reviewed version again.
        ed.review_action(&info(1.0, None), 0, HunkAction::Revert, &mut out);
        assert_eq!(std::fs::read_to_string(repo.0.join("run.py")).unwrap(), "import time\n\ndef run():\n    return 1\n");
        assert!(ed.tabs[0].hunks.is_empty());

        // The agent changes it again and you keep the whole file.
        std::fs::write(repo.0.join("run.py"), "import os\n\ndef run():\n    return 1\n").unwrap();
        ed.checked_at = f64::NEG_INFINITY;
        ed.check_disk(10.0);
        assert_eq!(ed.tabs[0].hunks.len(), 1, "reloaded from disk");
        ed.review_action(&info(11.0, None), usize::MAX, HunkAction::Ask, &mut out);
        assert_eq!(out.ask.as_deref(), Some("Why did you change run.py line 1?"));
        ed.review_action(&info(11.0, None), usize::MAX, HunkAction::Keep, &mut out);
        assert!(ed.tabs[0].hunks.is_empty());
        assert_ne!(review::tests::run(&repo.0, &["rev-parse", review::REVIEWED_REF]), start, "Keep moved the review");
    }

    #[test]
    fn saving_your_edit_leaves_only_the_agents_lines_tinted() {
        let (repo, mut ed) = reviewed_and_changed("editor-save");
        ed.open(Path::new("run.py"));
        let b = &mut ed.tabs[0];
        b.text = b.text.replacen("import time", "import time, os", 1);
        b.update_tints();
        assert_eq!(b.tints, vec![Tint { hunk: 0, lines: 3..5 }], "your unsaved line is not tinted");
        ed.save(1.0);
        assert_eq!(std::fs::read_to_string(repo.0.join("run.py")).unwrap(), ed.tabs[0].text);
        assert_eq!(ed.tabs[0].hunks, vec![Hunk { old: 3..4, new: 3..5 }], "only the agent's change is left");
        assert!(!ed.tabs[0].dirty());
    }

    #[test]
    fn the_tile_draws_without_a_window() {
        let (repo, mut ed) = reviewed_and_changed("editor-draw");
        std::fs::create_dir_all(repo.0.join("memory")).unwrap();
        std::fs::write(repo.0.join("memory/notes.md"), "# Notes\n").unwrap();
        ed.apply_actions(&info(1.0, None), &[patch(1.0, "run.py")]);
        ed.open(Path::new("memory/notes.md"));
        ed.active = 0;
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        let screen = Rect::from_min_size(egui::Pos2::ZERO, vec2(1200.0, 800.0));
        for _ in 0..2 {
            let raw = egui::RawInput { screen_rect: Some(screen), ..Default::default() };
            let mut output = ctx.run_ui(raw, |ui| {
                let head = Rect::from_min_size(pos2(100.0, 60.0), vec2(900.0, 38.0));
                let body = Rect::from_min_max(pos2(101.0, 99.0), pos2(999.0, 780.0));
                let out = ed.show(ui, &info(5.0, None), &[], head, body);
                assert!(!out.focused);
            });
            output.textures_delta.clear();
        }
        assert_eq!(ed.tabs.len(), 2);
        assert!(!ed.tabs[0].tints.is_empty());
    }
}
