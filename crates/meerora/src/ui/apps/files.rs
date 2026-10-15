//! The Files app of the agent view (D11, D21): a file manager for the
//! agent's directory. One folder at a time with a breadcrumb path, folders
//! first, sorted by name or by time. `.git` and the run logs stay hidden,
//! as in the editor's tree. Files the agent wrote in its last run carry a
//! dot in its colour.
//!
//! A double click or Enter opens a file: text and code in the editor,
//! results in the Viewer. Files dropped on the window while Files shows are
//! handed over: they are copied into the shown folder and never overwrite
//! anything (D25 asks that things reach an agent only on purpose). Delete
//! moves to the trash with `gio trash` and never removes a file for good.
//!
//! The agent can replace a folder with a symbolic link at any time. Rename,
//! new folder, hand-over and delete therefore open the shown folder step by
//! step without following links ([`Folder`]) and work through that handle,
//! so they never reach outside the agent's directory.

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, SystemTime};

use egui::{Align2, Color32, CornerRadius, Id, Key, Modifiers, Rect, Sense, Shape, Stroke, StrokeKind, Ui, pos2, vec2};

use super::viewer::{self, Kind, Route};
use crate::engine::Core;
use crate::theme::{self, c, ca};
use crate::ui::editor::files as tree;
use crate::ui::{AppTab, ButtonStyle, button, fit, pill};
use crate::world::ActionEntry;

const BAR_H: f32 = 36.0;
const HEADER_H: f32 = 26.0;
const ROW_H: f32 = 28.0;
const STATUS_H: f32 = 26.0;
const CONFIRM_H: f32 = 44.0;
/// Seconds between scans of the shown folder.
const SCAN_EVERY: f64 = 1.5;
/// Seconds between reads of the agent's `state.json` for its last run.
const STATE_EVERY: f64 = 5.0;
/// Entries a folder lists at most, so a huge folder cannot stall the app.
pub const MAX_ITEMS: usize = 5000;

const BAR_BG: [u8; 3] = [0x0F, 0x12, 0x18];
const DIM: [u8; 3] = [0x8A, 0x92, 0xA3];
const FAINT: [u8; 3] = [0x4A, 0x51, 0x60];
const DANGER: [u8; 3] = [0xE0, 0x67, 0x5A];

/// One entry of a folder.
#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    pub name: String,
    pub dir: bool,
    /// Bytes for a file, entries for a folder.
    pub size: u64,
    pub modified: Option<SystemTime>,
    /// The newest modification among a folder's entries.
    pub newest: Option<SystemTime>,
}

/// A folder's entries, or why it cannot be read.
type Listing = Result<Vec<Item>, String>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sort {
    Name,
    /// Newest first.
    Time,
}

/// The entries of `rel` inside `root`, unsorted. Hidden entries follow the
/// editor's tree: `.git` everywhere, `logs` at the top. Symbolic links count
/// as files when they point at one and are left out when they point at a
/// folder, so the view never leaves the agent's directory.
pub fn list(root: &Path, rel: &Path) -> Listing {
    let depth = rel.components().count();
    let read = std::fs::read_dir(root.join(rel)).map_err(|e| format!("Cannot read {} ({e}).", shown_path(rel)))?;
    let mut items = Vec::new();
    for e in read.flatten() {
        if items.len() >= MAX_ITEMS {
            break;
        }
        let name = e.file_name().to_string_lossy().into_owned();
        let Ok(kind) = e.file_type() else { continue };
        if tree::hidden(&name, depth) {
            continue;
        }
        let path = e.path();
        let Ok(meta) = std::fs::metadata(&path) else { continue };
        let dir = kind.is_dir();
        if !dir && !meta.is_file() {
            continue;
        }
        let (size, newest) = if dir { folder_info(&path, depth + 1) } else { (meta.len(), None) };
        items.push(Item { name, dir, size, modified: meta.modified().ok(), newest });
    }
    Ok(items)
}

/// Entries of a folder and the newest modification among them.
fn folder_info(path: &Path, depth: usize) -> (u64, Option<SystemTime>) {
    let Ok(read) = std::fs::read_dir(path) else { return (0, None) };
    let mut n = 0;
    let mut newest = None;
    for e in read.flatten().take(MAX_ITEMS) {
        if tree::hidden(&e.file_name().to_string_lossy(), depth) {
            continue;
        }
        n += 1;
        if let Ok(modified) = e.metadata().and_then(|m| m.modified()) {
            newest = newest.max(Some(modified));
        }
    }
    (n, newest)
}

/// Folders first, then by name or newest first. `reverse` turns the order
/// around within folders and files.
pub fn sort_items(items: &mut [Item], sort: Sort, reverse: bool) {
    let by_name = |a: &Item, b: &Item| a.name.to_lowercase().cmp(&b.name.to_lowercase()).then_with(|| a.name.cmp(&b.name));
    items.sort_by(|a, b| {
        let order = match sort {
            Sort::Name => by_name(a, b),
            Sort::Time => b.modified.cmp(&a.modified).then_with(|| by_name(a, b)),
        };
        b.dir.cmp(&a.dir).then(if reverse { order.reverse() } else { order })
    });
}

/// `name`, or the first free "name (2).ext", "name (3).ext" in `dir`. An
/// existing " (n)" counts up instead of growing.
pub fn unique_name(dir: &Path, name: &str) -> String {
    let taken = |n: &str| std::fs::symlink_metadata(dir.join(n)).is_ok();
    if !taken(name) {
        return name.to_string();
    }
    // A leading dot starts a hidden name, not an extension.
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => name.split_at(i),
        _ => (name, ""),
    };
    let base = match stem.strip_suffix(')').and_then(|s| s.rsplit_once(" (")) {
        Some((base, n)) if !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()) => base,
        _ => stem,
    };
    (2..).map(|n| format!("{base} ({n}){ext}")).find(|candidate| !taken(candidate)).unwrap_or_default()
}

/// A folder held open. Paths below it go through `/proc/self/fd`, so a
/// symbolic link that later takes the place of the folder or of one above it
/// cannot redirect them.
pub(crate) struct Folder(std::fs::File);

impl Folder {
    /// The folder at `path`, which the caller trusts, such as the agent's directory.
    pub(crate) fn at(path: &Path) -> io::Result<Self> {
        Self::open(path, 0)
    }

    /// The folder `rel` below `root`. Every step must be a real folder, a
    /// symbolic link anywhere on the way is refused.
    pub(crate) fn below(root: &Path, rel: &Path) -> io::Result<Self> {
        let mut folder = Self::at(root)?;
        for part in rel.components() {
            match part {
                Component::Normal(name) => folder = folder.child(name)?,
                Component::CurDir => {}
                _ => return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("{} leaves the folder", rel.display()))),
            }
        }
        Ok(folder)
    }

    /// The real subfolder `name`. Links and anything that is not a folder
    /// are refused at once, nothing waits on a pipe.
    pub(crate) fn child(&self, name: &OsStr) -> io::Result<Self> {
        Self::open(&self.join(name), libc::O_NOFOLLOW).map_err(|e| match e.raw_os_error() {
            Some(libc::ELOOP | libc::ENOTDIR) => {
                io::Error::new(io::ErrorKind::InvalidInput, format!("{} is a link or not a folder", name.to_string_lossy()))
            }
            _ => e,
        })
    }

    fn open(path: &Path, flags: i32) -> io::Result<Self> {
        let file = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY | libc::O_NONBLOCK | flags).open(path)?;
        Ok(Self(file))
    }

    /// `name` inside this folder.
    pub(crate) fn join(&self, name: impl AsRef<Path>) -> PathBuf {
        PathBuf::from(format!("/proc/self/fd/{}", self.0.as_raw_fd())).join(name)
    }
}

/// Copies the file or folder `src` into the folder `dest` under a free name
/// and returns that name. Nothing that exists is ever overwritten. Symbolic
/// links inside a copied folder are left out.
pub(crate) fn copy_into(src: &Path, dest: &Folder) -> io::Result<String> {
    let name = src.file_name().map(|n| n.to_string_lossy().into_owned()).ok_or_else(|| io::Error::other("no file name"))?;
    let meta = std::fs::metadata(src)?;
    if meta.is_dir() && dest.join("").canonicalize().ok().zip(src.canonicalize().ok()).is_some_and(|(d, s)| d.starts_with(s)) {
        return Err(io::Error::other("a folder cannot go into itself"));
    }
    let target_name = unique_name(&dest.join(""), &name);
    if meta.is_dir() {
        copy_dir(src, dest, OsStr::new(&target_name))?
    } else {
        copy_file(src, &dest.join(&target_name))?
    }
    Ok(target_name)
}

fn copy_file(src: &Path, target: &Path) -> io::Result<()> {
    let mut from = std::fs::File::open(src)?;
    let mut to = std::fs::OpenOptions::new().write(true).create_new(true).open(target)?;
    io::copy(&mut from, &mut to)?;
    if let Ok(meta) = from.metadata() {
        let _ = to.set_permissions(meta.permissions());
    }
    Ok(())
}

/// Copies the folder `src` as `name` into `parent`. The new folder is opened
/// without following links, so the agent cannot swap it for a link while
/// the copy runs.
fn copy_dir(src: &Path, parent: &Folder, name: &OsStr) -> io::Result<()> {
    std::fs::create_dir(parent.join(name))?;
    let target = parent.child(name)?;
    for e in std::fs::read_dir(src)? {
        let e = e?;
        let kind = e.file_type()?;
        if kind.is_dir() {
            copy_dir(&e.path(), &target, &e.file_name())?;
        } else if kind.is_file() {
            copy_file(&e.path(), &target.join(e.file_name()))?;
        }
    }
    Ok(())
}

/// The files an action summary names inside `dir`. Summaries look like
/// `run.py +5 -1`, `memory/notes.md` or `a.txt, b.txt`.
pub(crate) fn named_files(dir: &Path, summary: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for part in summary.split(", ").map(str::trim) {
        // "my notes.md +3 -1": the counts come off, the spaces of the name stay.
        let without_counts = match part.rsplitn(3, ' ').collect::<Vec<_>>().as_slice() {
            [del, add, name] if add.starts_with('+') && del.starts_with('-') => *name,
            _ => part,
        };
        let first = part.split_whitespace().next().unwrap_or_default();
        let found = [part, without_counts, first].into_iter().filter(|c| !c.is_empty()).find_map(|c| tree::inside(dir, c));
        if let Some(rel) = found
            && !out.contains(&rel)
        {
            out.push(rel);
        }
    }
    out
}

/// A short time for a file: "just now", "12 min ago", "14:02",
/// "yesterday 14:02", "12 Oct" or "12 Oct 2025", in local time.
pub(crate) fn when(t: SystemTime, now: SystemTime) -> String {
    let secs = now.duration_since(t).map_or(0, |d| d.as_secs());
    if secs < 60 {
        return "just now".into();
    }
    if secs < 3600 {
        return format!("{} min ago", secs / 60);
    }
    let (Ok(t), Ok(now)) = (jiff::Timestamp::try_from(t), jiff::Timestamp::try_from(now)) else { return String::new() };
    let tz = jiff::tz::TimeZone::system();
    let (t, now) = (t.to_zoned(tz.clone()), now.to_zoned(tz));
    if t.date() == now.date() {
        t.strftime("%H:%M").to_string()
    } else if now.date().yesterday().is_ok_and(|y| y == t.date()) {
        t.strftime("yesterday %H:%M").to_string()
    } else if t.year() == now.year() {
        t.strftime("%-d %b").to_string()
    } else {
        t.strftime("%-d %b %Y").to_string()
    }
}

/// Opens a path or URL with the desktop's default application (`xdg-open`)
/// without waiting for it.
pub(crate) fn open_external(path: &Path) -> Result<(), String> {
    let child = std::process::Command::new("xdg-open")
        .arg(path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("Cannot run xdg-open ({e})."))?;
    // Reaped on a thread, so the UI never waits for it.
    let _ = std::thread::Builder::new().name("xdg-open".into()).spawn(move || {
        let mut child = child;
        let _ = child.wait();
    });
    Ok(())
}

/// Whether `gio` is on the PATH, for moving files to the trash.
fn has_gio() -> bool {
    static GIO: OnceLock<bool> = OnceLock::new();
    *GIO.get_or_init(|| std::env::var_os("PATH").is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join("gio").is_file())))
}

const NO_GIO: &str = "Moving to the trash needs gio from GLib, which is not installed. meerora never deletes files for good.";

/// Opens `rel` like a double click in Files: text and code in the editor,
/// results in the Viewer.
pub fn open_path(core: &mut Core, agent: &str, rel: &Path) {
    let Some(dir) = core.agent_dir(agent).map(|d| d.path.clone()) else { return };
    match viewer::route(&dir.join(rel)) {
        Route::Editor => crate::ui::editor::open_file(core, agent, rel),
        Route::Viewer(_) => viewer::show_file(core, agent, rel),
    }
}

/// `key` is the instance key of a Files app (`ui::instance_key`).
fn list_id(key: &str) -> Id {
    Id::new(("files-list", key))
}

fn rename_id(key: &str) -> Id {
    Id::new(("files-rename", key))
}

/// Whether F2 belongs to a Files app of `agent`, which renames with it,
/// instead of switching between tiled and fullscreen.
pub fn takes_f2(core: &Core, ctx: &egui::Context, agent: &str) -> bool {
    core.ui.files.agents.keys().filter(|(a, _)| a == agent).any(|(a, n)| {
        let key = crate::ui::instance_key(a, *n);
        ctx.memory(|m| m.has_focus(list_id(&key)) || m.has_focus(rename_id(&key)))
    })
}

/// The Files state of every Files instance that was shown, by agent and
/// instance number. Each instance has its own folder and selection.
#[derive(Default)]
pub struct Files {
    agents: HashMap<(String, u32), FilesTile>,
}

/// Draws Files instance `n` of agent `index`. Returns true while it has
/// keyboard focus, so Esc stays in the tile.
pub fn tile(ui: &mut Ui, core: &mut Core, index: usize, n: u32, head: Rect, body: Rect) -> bool {
    let a = &core.world.agents[index];
    let id = a.id.clone();
    let info = Info {
        id: crate::ui::instance_key(&id, n),
        name: a.name.clone(),
        color: a.color,
        now: core.world.time,
        run: a.run.as_ref().map(|r| r.id.clone()),
        external: crate::ui::index::external(core, &id).is_some(),
    };
    let actions: Vec<ActionEntry> = a.actions.iter().filter(|e| e.tool == "editor.patch").cloned().collect();
    let Some(dir) = core.agent_dir(&id).map(|d| d.path.clone()) else {
        super::super::agent::placeholder(ui, body, "No directory", "This agent has no directory to show.");
        return false;
    };
    let key = (id.clone(), n);
    let mut t = core.ui.files.agents.remove(&key).filter(|t| t.dir == dir).unwrap_or_else(|| FilesTile::new(dir));
    let out = t.show(ui, &info, &actions, head, body);
    core.ui.files.agents.insert(key, t);
    if !out.yours.is_empty() {
        viewer::mark_yours(core, &id, &out.yours);
    }
    if let Some(rel) = out.open {
        open_path(core, &id, &rel);
    }
    if let Some((text, error)) = out.toast {
        core.ui.toast(info.now, text, error);
    }
    if out.clicked {
        core.ui.tab = AppTab::Files(n);
    }
    out.focused
}

struct Info {
    /// The instance key for egui ids (`ui::instance_key`), the agent id for instance 1.
    id: String,
    name: String,
    color: [u8; 3],
    now: f64,
    run: Option<String>,
    external: bool,
}

#[derive(Default)]
struct Out {
    focused: bool,
    clicked: bool,
    open: Option<PathBuf>,
    toast: Option<(String, bool)>,
    /// Paths you created, which the Viewer must not call new from the agent.
    yours: Vec<PathBuf>,
}

/// Work done on a thread: handing over and moving to the trash.
enum Done {
    HandedOver { into: PathBuf, names: Vec<String>, failed: Vec<String> },
    Trashed { name: String, result: Result<(), String> },
}

/// How a row is marked as the agent's work.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mark {
    None,
    /// The agent wrote this file in its last run.
    New,
    /// The folder holds such files.
    Inside,
}

struct FilesTile {
    dir: PathBuf,
    /// The shown folder, relative to the agent directory.
    cwd: PathBuf,
    items: Vec<Item>,
    error: Option<String>,
    sort: Sort,
    reverse: bool,
    /// Name of the selected entry of the shown folder.
    selected: Option<String>,
    scan: Option<(PathBuf, Receiver<Listing>)>,
    scanned_at: f64,
    /// The entry being renamed and the text of the field.
    renaming: Option<(String, String)>,
    /// The entry waiting for the trash confirmation.
    confirm: Option<String>,
    /// A short note in the status bar: text, whether it is an error, since when.
    message: Option<(String, bool, f64)>,
    jobs: Vec<Receiver<Done>>,
    /// Files the agent patched since its last run began, from its actions.
    recent: HashSet<PathBuf>,
    /// Paths you made here, never marked as the agent's.
    yours: HashSet<PathBuf>,
    seen_action: Option<f64>,
    /// The run the app saw start, and when.
    run_seen: Option<(String, SystemTime)>,
    /// Start of the agent's last finished run, from its `state.json`.
    last_run: Option<SystemTime>,
    state_read_at: f64,
    /// Scroll position and height of the list, from the last frame.
    offset: f32,
    view_h: f32,
    scroll_to: Option<f32>,
}

impl FilesTile {
    fn new(dir: PathBuf) -> Self {
        let mut t = Self {
            dir,
            cwd: PathBuf::new(),
            items: Vec::new(),
            error: None,
            sort: Sort::Name,
            reverse: false,
            selected: None,
            scan: None,
            scanned_at: 0.0,
            renaming: None,
            confirm: None,
            message: None,
            jobs: Vec::new(),
            recent: HashSet::new(),
            yours: HashSet::new(),
            seen_action: None,
            run_seen: None,
            last_run: None,
            state_read_at: f64::NEG_INFINITY,
            offset: 0.0,
            view_h: 0.0,
            scroll_to: None,
        };
        t.rescan_now();
        t
    }

    /// Lists the shown folder at once, for navigation and after changes.
    fn rescan_now(&mut self) {
        let result = list(&self.dir, &self.cwd);
        self.apply_listing(result);
        self.scan = None;
    }

    fn apply_listing(&mut self, result: Listing) {
        match result {
            Ok(mut items) => {
                sort_items(&mut items, self.sort, self.reverse);
                self.items = items;
                self.error = None;
            }
            Err(e) => {
                self.items.clear();
                self.error = Some(e);
            }
        }
        if let Some(name) = &self.selected
            && !self.items.iter().any(|i| &i.name == name)
        {
            self.selected = None;
        }
    }

    fn go(&mut self, rel: PathBuf, select: Option<String>) {
        self.cwd = rel;
        self.selected = select;
        self.renaming = None;
        self.confirm = None;
        self.offset = 0.0;
        self.scroll_to = Some(0.0);
        self.rescan_now();
        if self.selected.is_some() {
            self.reveal_selected();
        }
    }

    fn go_up(&mut self) {
        let Some(parent) = self.cwd.parent().map(Path::to_path_buf) else { return };
        let from = self.cwd.file_name().map(|n| n.to_string_lossy().into_owned());
        if self.cwd.as_os_str().is_empty() {
            return;
        }
        self.go(parent, from);
    }

    fn poll(&mut self, info: &Info, out: &mut Out) {
        if let Some((cwd, rx)) = &self.scan {
            match rx.try_recv() {
                Ok(result) => {
                    let current = *cwd == self.cwd;
                    self.scan = None;
                    if current {
                        self.apply_listing(result);
                    }
                }
                Err(mpsc::TryRecvError::Disconnected) => self.scan = None,
                Err(mpsc::TryRecvError::Empty) => {}
            }
        } else if info.now - self.scanned_at > SCAN_EVERY {
            self.scanned_at = info.now;
            let (dir, cwd) = (self.dir.clone(), self.cwd.clone());
            let (tx, rx) = mpsc::channel();
            let spawned = std::thread::Builder::new().name("files-scan".into()).spawn(move || {
                let _ = tx.send(list(&dir, &cwd));
            });
            match spawned {
                Ok(_) => self.scan = Some((self.cwd.clone(), rx)),
                Err(e) => tracing::warn!("cannot scan the agent directory: {e}"),
            }
        }
        let mut done = Vec::new();
        self.jobs.retain(|rx| match rx.try_recv() {
            Ok(d) => {
                done.push(d);
                false
            }
            Err(mpsc::TryRecvError::Empty) => true,
            Err(mpsc::TryRecvError::Disconnected) => false,
        });
        for d in done {
            match d {
                Done::HandedOver { into, names, failed } => {
                    for name in &names {
                        let rel = into.join(name);
                        self.yours.insert(rel.clone());
                        out.yours.push(rel);
                    }
                    if into == self.cwd {
                        self.rescan_now();
                        self.selected = names.first().cloned();
                        self.reveal_selected();
                    }
                    let place = if into.as_os_str().is_empty() { "its folder".to_string() } else { format!("{}/", into.display()) };
                    let text = match names.as_slice() {
                        [] => String::new(),
                        [one] => format!("Handed over {one} to {}, into {place}.", info.name),
                        many => format!("Handed over {} files to {}, into {place}: {}.", many.len(), info.name, many.join(", ")),
                    };
                    if failed.is_empty() {
                        out.toast = Some((text, false));
                    } else {
                        let errors = format!("Could not hand over {}.", failed.join(", "));
                        out.toast = Some((if text.is_empty() { errors } else { format!("{text} {errors}") }, true));
                    }
                }
                Done::Trashed { name, result } => {
                    self.rescan_now();
                    self.message = Some(match result {
                        Ok(()) => (format!("Moved {name} to the trash"), false, info.now),
                        Err(e) => (e, true, info.now),
                    });
                }
            }
        }
    }

    /// The agent's latest files: patched since its last run began, or
    /// changed since then.
    fn track(&mut self, info: &Info, actions: &[ActionEntry]) {
        if let Some(run) = &info.run
            && self.run_seen.as_ref().is_none_or(|(id, _)| id != run)
        {
            self.run_seen = Some((run.clone(), SystemTime::now()));
            self.recent.clear();
        }
        for e in actions.iter().filter(|e| self.seen_action.is_none_or(|s| e.at > s)) {
            self.recent.extend(named_files(&self.dir, e.target()));
        }
        if let Some(last) = actions.iter().map(|e| e.at).reduce(f64::max) {
            self.seen_action = Some(self.seen_action.map_or(last, |s| s.max(last)));
        }
        if info.now - self.state_read_at > STATE_EVERY {
            self.state_read_at = info.now;
            self.last_run = last_run_start(&self.dir);
        }
    }

    /// Since when changed files count as the agent's.
    fn since(&self, info: &Info) -> Option<SystemTime> {
        let observed = self.run_seen.as_ref().map(|(_, at)| *at);
        if info.run.is_some() { observed } else { self.last_run.or(observed) }
    }

    fn mark(&self, item: &Item, since: Option<SystemTime>) -> Mark {
        let rel = self.cwd.join(&item.name);
        // The daemon's own files are not the agent's work.
        if self.yours.contains(&rel) || (self.cwd.as_os_str().is_empty() && item.name.starts_with("state.json")) {
            return Mark::None;
        }
        let changed = |t: Option<SystemTime>| since.is_some_and(|s| t.is_some_and(|t| t >= s));
        if item.dir {
            let inside = self.recent.iter().any(|p| p.starts_with(&rel)) || changed(item.newest);
            if inside { Mark::Inside } else { Mark::None }
        } else if self.recent.contains(&rel) || changed(item.modified) {
            Mark::New
        } else {
            Mark::None
        }
    }

    fn selected_index(&self) -> Option<usize> {
        let name = self.selected.as_ref()?;
        self.items.iter().position(|i| &i.name == name)
    }

    fn reveal_selected(&mut self) {
        let Some(i) = self.selected_index() else { return };
        let y = i as f32 * ROW_H;
        if y < self.offset {
            self.scroll_to = Some(y);
        } else if y + ROW_H > self.offset + self.view_h && self.view_h > 0.0 {
            self.scroll_to = Some(y + ROW_H - self.view_h);
        }
    }

    /// Opens an entry: a folder in place, a file in the editor or the Viewer.
    fn activate(&mut self, name: &str, out: &mut Out) {
        let Some(item) = self.items.iter().find(|i| i.name == name) else { return };
        let rel = self.cwd.join(&item.name);
        if item.dir {
            self.go(rel, None);
        } else {
            out.open = Some(rel);
        }
    }

    fn start_rename(&mut self, name: &str, ui: &Ui, info: &Info) {
        self.confirm = None;
        self.renaming = Some((name.to_string(), name.to_string()));
        ui.memory_mut(|m| m.request_focus(rename_id(&info.id)));
    }

    fn finish_rename(&mut self, info: &Info) {
        let Some((from, to)) = self.renaming.take() else { return };
        let to = to.trim().to_string();
        if to == from || to.is_empty() {
            return;
        }
        if to.contains('/') || to == "." || to == ".." || to.contains('\0') {
            self.message = Some((format!("{to} is not a valid name."), true, info.now));
            return;
        }
        let folder = match Folder::below(&self.dir, &self.cwd) {
            Ok(f) => f,
            Err(e) => {
                self.message = Some((format!("Cannot rename {from} ({e})."), true, info.now));
                return;
            }
        };
        if std::fs::symlink_metadata(folder.join(&to)).is_ok() {
            self.message = Some((format!("{to} exists already. Choose another name."), true, info.now));
            return;
        }
        match std::fs::rename(folder.join(&from), folder.join(&to)) {
            Ok(()) => {
                self.yours.insert(self.cwd.join(&to));
                self.message = Some((format!("Renamed {from} to {to}"), false, info.now));
                self.rescan_now();
                self.selected = Some(to);
                self.reveal_selected();
            }
            Err(e) => self.message = Some((format!("Cannot rename {from} ({e})."), true, info.now)),
        }
    }

    fn new_folder(&mut self, ui: &Ui, info: &Info) {
        let folder = match Folder::below(&self.dir, &self.cwd) {
            Ok(f) => f,
            Err(e) => {
                self.message = Some((format!("Cannot create a folder here ({e})."), true, info.now));
                return;
            }
        };
        let name = unique_name(&folder.join(""), "New folder");
        match std::fs::create_dir(folder.join(&name)) {
            Ok(()) => {
                self.yours.insert(self.cwd.join(&name));
                self.rescan_now();
                self.selected = Some(name.clone());
                self.reveal_selected();
                self.start_rename(&name, ui, info);
            }
            Err(e) => self.message = Some((format!("Cannot create a folder here ({e})."), true, info.now)),
        }
    }

    fn copy_path(&mut self, ui: &Ui, name: Option<&str>, info: &Info) {
        let path = match name {
            Some(n) => self.dir.join(&self.cwd).join(n),
            None => self.dir.join(&self.cwd),
        };
        ui.ctx().copy_text(path.display().to_string());
        self.message = Some((format!("Copied {}", path.display()), false, info.now));
    }

    fn ask_trash(&mut self, name: &str, info: &Info) {
        if has_gio() {
            self.renaming = None;
            self.confirm = Some(name.to_string());
        } else {
            self.message = Some((NO_GIO.to_string(), true, info.now));
        }
    }

    fn trash(&mut self, name: &str) {
        let path = self.dir.join(&self.cwd).join(name);
        let (root, cwd) = (self.dir.clone(), self.cwd.clone());
        let name = name.to_string();
        let (tx, rx) = mpsc::channel();
        let spawned = std::thread::Builder::new().name("files-trash".into()).spawn(move || {
            // gio runs in its own process, so the folders are checked for links first.
            if let Err(e) = Folder::below(&root, &cwd) {
                let _ = tx.send(Done::Trashed { name, result: Err(format!("Cannot move it to the trash ({e}).")) });
                return;
            }
            let result = std::process::Command::new("gio")
                .arg("trash")
                .arg("--")
                .arg(&path)
                .stdin(std::process::Stdio::null())
                .output()
                .map_err(|e| format!("Cannot run gio ({e})."))
                .and_then(|o| {
                    if o.status.success() {
                        Ok(())
                    } else {
                        let why = String::from_utf8_lossy(&o.stderr).trim().to_string();
                        Err(format!("Cannot move {} to the trash: {why}", path.file_name().unwrap_or_default().to_string_lossy()))
                    }
                });
            let _ = tx.send(Done::Trashed { name, result });
        });
        if spawned.is_ok() {
            self.jobs.push(rx);
        }
    }

    /// Copies dropped files into the shown folder, on a thread.
    fn hand_over(&mut self, paths: Vec<PathBuf>) {
        let into = self.cwd.clone();
        // Opened step by step without following links, before the copy starts.
        let folder = Folder::below(&self.dir, &into);
        let (tx, rx) = mpsc::channel();
        let spawned = std::thread::Builder::new().name("files-hand-over".into()).spawn(move || {
            let mut names = Vec::new();
            let mut failed = Vec::new();
            for p in paths {
                let copied = match &folder {
                    Ok(folder) => copy_into(&p, folder),
                    Err(e) => Err(io::Error::new(e.kind(), e.to_string())),
                };
                match copied {
                    Ok(name) => names.push(name),
                    Err(e) => failed.push(format!("{} ({e})", p.file_name().unwrap_or_default().to_string_lossy())),
                }
            }
            let _ = tx.send(Done::HandedOver { into, names, failed });
        });
        if spawned.is_ok() {
            self.jobs.push(rx);
        }
    }

    fn show(&mut self, ui: &mut Ui, info: &Info, actions: &[ActionEntry], head: Rect, body: Rect) -> Out {
        let mut out = Out::default();
        // Focus at the start counts too: the Esc that only drops it must not leave the view.
        let had_focus = ui.memory(|m| m.has_focus(list_id(&info.id)) || m.has_focus(rename_id(&info.id)))
            || self.renaming.is_some()
            || self.confirm.is_some();
        self.poll(info, &mut out);
        self.track(info, actions);
        if !self.jobs.is_empty() || self.scan.is_some() {
            ui.ctx().request_repaint_after(Duration::from_millis(100));
        }

        // Hand over: files dropped on the window while Files shows (D25).
        let (hovering, dropped) =
            ui.input(|i| (!i.raw.hovered_files.is_empty(), i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).collect::<Vec<_>>()));
        if !dropped.is_empty() && self.error.is_none() {
            self.hand_over(dropped);
        }

        self.draw_head(ui, info, head);
        let bar = Rect::from_min_size(body.min, vec2(body.width(), BAR_H));
        let status = Rect::from_min_max(pos2(body.min.x, body.max.y - STATUS_H), body.max);
        let confirm =
            self.confirm.as_ref().map(|_| Rect::from_min_max(pos2(body.min.x, status.min.y - CONFIRM_H), pos2(body.max.x, status.min.y)));
        let header = Rect::from_min_size(pos2(body.min.x, bar.max.y), vec2(body.width(), HEADER_H));
        let list_r = Rect::from_min_max(pos2(body.min.x, header.max.y), pos2(body.max.x, confirm.map_or(status.min.y, |r| r.min.y)));
        let columns = Columns::for_width(body.width());

        self.draw_bar(ui, info, bar);
        self.draw_header(ui, info, header, columns);
        self.draw_list(ui, info, list_r, columns, &mut out);
        if let Some(r) = confirm {
            self.draw_confirm(ui, r);
        }
        self.draw_status(ui, info, status);
        self.keys(ui, info, &mut out);

        if hovering {
            let painter = ui.painter_at(body);
            painter.rect(
                body.shrink(6.0),
                CornerRadius::same(10),
                ca(theme::NEEDS_YOU, 22),
                Stroke::new(1.5, c(theme::NEEDS_YOU)),
                StrokeKind::Inside,
            );
            let into = if self.cwd.as_os_str().is_empty() { "its folder".to_string() } else { format!("{}/", self.cwd.display()) };
            painter.text(
                body.center() - vec2(0.0, 12.0),
                Align2::CENTER_CENTER,
                format!("Drop to hand over to {}", info.name),
                theme::ui_semibold(17.0),
                c(theme::TEXT),
            );
            painter.text(
                body.center() + vec2(0.0, 14.0),
                Align2::CENTER_CENTER,
                format!("Copies go into {into}"),
                theme::ui(13.5),
                c(theme::TEXT_2),
            );
        }

        let focus = ui.memory(|m| m.has_focus(list_id(&info.id)) || m.has_focus(rename_id(&info.id)));
        out.focused = focus || had_focus || self.renaming.is_some() || self.confirm.is_some();
        if ui.rect_contains_pointer(body) && ui.input(|i| i.pointer.any_pressed()) {
            out.clicked = true;
        }
        out
    }

    fn keys(&mut self, ui: &mut Ui, info: &Info, out: &mut Out) {
        let id = list_id(&info.id);
        if self.confirm.is_some() {
            let (yes, no) = ui.input_mut(|i| (i.consume_key(Modifiers::NONE, Key::Enter), i.consume_key(Modifiers::NONE, Key::Escape)));
            if yes && let Some(name) = self.confirm.take() {
                self.trash(&name);
            }
            if no {
                self.confirm = None;
            }
            return;
        }
        if !ui.memory(|m| m.has_focus(id)) {
            return;
        }
        ui.memory_mut(|m| {
            m.set_focus_lock_filter(id, egui::EventFilter { tab: false, horizontal_arrows: true, vertical_arrows: true, escape: true })
        });
        let pressed = |ui: &mut Ui, key: Key| ui.input_mut(|i| i.consume_key(Modifiers::NONE, key));
        let n = self.items.len();
        let at = self.selected_index();
        let mut moved = None;
        if pressed(ui, Key::ArrowDown) && n > 0 {
            moved = Some(at.map_or(0, |i| (i + 1).min(n - 1)));
        }
        if pressed(ui, Key::ArrowUp) && n > 0 {
            moved = Some(at.map_or(n - 1, |i| i.saturating_sub(1)));
        }
        if pressed(ui, Key::Home) && n > 0 {
            moved = Some(0);
        }
        if pressed(ui, Key::End) && n > 0 {
            moved = Some(n - 1);
        }
        if pressed(ui, Key::PageDown) && n > 0 {
            let page = (self.view_h / ROW_H).floor().max(1.0) as usize;
            moved = Some(at.map_or(0, |i| (i + page).min(n - 1)));
        }
        if pressed(ui, Key::PageUp) && n > 0 {
            let page = (self.view_h / ROW_H).floor().max(1.0) as usize;
            moved = Some(at.map_or(0, |i| i.saturating_sub(page)));
        }
        if let Some(i) = moved {
            self.selected = Some(self.items[i].name.clone());
            self.reveal_selected();
        }
        if pressed(ui, Key::Enter)
            && let Some(name) = self.selected.clone()
        {
            self.activate(&name, out);
        }
        if pressed(ui, Key::Backspace) {
            self.go_up();
        }
        if pressed(ui, Key::F2)
            && let Some(name) = self.selected.clone()
        {
            self.start_rename(&name, ui, info);
        }
        if pressed(ui, Key::Delete)
            && let Some(name) = self.selected.clone()
        {
            self.ask_trash(&name, info);
        }
        if pressed(ui, Key::Escape) {
            ui.memory_mut(|m| m.surrender_focus(id));
        }
    }

    fn draw_head(&mut self, ui: &mut Ui, info: &Info, head: Rect) {
        let y = head.center().y;
        let chip = pill(ui.painter(), pos2(head.max.x - 12.0, y), "YOU", [0xB9, 0xBF, 0xCC]);
        // The breadcrumb: the agent's folder, then each folder down to the shown one.
        let root = self.dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| info.id.clone());
        let mut parts: Vec<(String, PathBuf)> = vec![(root, PathBuf::new())];
        let mut at = PathBuf::new();
        for part in self.cwd.components() {
            at.push(part);
            parts.push((part.as_os_str().to_string_lossy().into_owned(), at.clone()));
        }
        let font = theme::mono(12.0);
        let painter = ui.painter();
        let sep_w = painter.layout_no_wrap(" / ".into(), font.clone(), Color32::WHITE).size().x;
        let widths: Vec<f32> =
            parts.iter().map(|(p, _)| painter.layout_no_wrap(p.clone(), font.clone(), Color32::WHITE).size().x + 8.0).collect();
        let x0 = head.min.x + 62.0;
        let room = chip.min.x - 12.0 - x0;
        // Leading folders give way to "…" when the path is too long.
        let mut first = 0;
        let total =
            |from: usize| widths[from..].iter().sum::<f32>() + sep_w * (widths.len() - from) as f32 + if from > 0 { 24.0 } else { 0.0 };
        while first + 1 < parts.len() && total(first) > room {
            first += 1;
        }
        let mut x = x0;
        let mut go = None;
        if first > 0 {
            ui.painter().text(pos2(x + 4.0, y), Align2::LEFT_CENTER, "…", font.clone(), c(DIM));
            x += 16.0;
            ui.painter().text(pos2(x, y), Align2::LEFT_CENTER, " /", font.clone(), c(FAINT));
            x += sep_w;
        }
        let last = parts.len() - 1;
        for (i, (part, rel)) in parts.iter().enumerate().skip(first) {
            let w = widths[i].min((chip.min.x - 12.0 - x).max(0.0));
            if w <= 8.0 {
                break;
            }
            let r = Rect::from_min_size(pos2(x, y - 11.0), vec2(w, 22.0));
            let resp = ui.interact(r, Id::new(("files-crumb", &info.id, i)), Sense::click());
            let painter = ui.painter();
            if resp.hovered() && i != last {
                painter.rect_filled(r, CornerRadius::same(5), c([0x1A, 0x1F, 0x2A]));
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
            let text = fit(painter, part, font.clone(), w - 8.0);
            painter.text(pos2(x + 4.0, y), Align2::LEFT_CENTER, text, font.clone(), if i == last { c(theme::TEXT) } else { c(DIM) });
            if resp.clicked() && i != last {
                go = Some(rel.clone());
            }
            x += w;
            if i != last {
                painter.text(pos2(x, y), Align2::LEFT_CENTER, " / ", font.clone(), c(FAINT));
                x += sep_w;
            }
        }
        if let Some(rel) = go {
            let from =
                self.cwd.strip_prefix(&rel).ok().and_then(|r| r.components().next()).map(|c| c.as_os_str().to_string_lossy().into_owned());
            self.go(rel, from);
        }
    }

    fn draw_bar(&mut self, ui: &mut Ui, info: &Info, r: Rect) {
        let painter = ui.painter_at(r);
        painter.rect_filled(r, CornerRadius::ZERO, c(BAR_BG));
        super::super::hline(&painter, r.max.y - 0.5, r.min.x, r.max.x);
        let y = r.center().y;
        let selected = self.selected.clone();
        let has = selected.is_some();
        let trash_tip = if has_gio() { "Move the selected entry to the trash (Delete)" } else { NO_GIO };
        // Buttons on the left drop out from the end when the tile is narrow.
        let manager_w = 112.0;
        let left_room = r.width() - 16.0 - manager_w - 12.0;
        let buttons: [(&str, f32, bool, &str); 5] = [
            ("Up", 40.0, !self.cwd.as_os_str().is_empty(), "The folder above (Backspace)"),
            ("New folder", 92.0, self.error.is_none(), "Create a folder here"),
            ("Rename", 70.0, has, "Rename the selected entry (F2)"),
            ("Copy path", 82.0, true, "Copy the path of the selection, or of this folder"),
            ("Move to trash", 108.0, has && has_gio(), trash_tip),
        ];
        let mut x = r.min.x + 8.0;
        let mut used = 0.0;
        for (i, (label, w, enabled, tip)) in buttons.into_iter().enumerate() {
            if used + w > left_room {
                break;
            }
            let rect = Rect::from_min_size(pos2(x, y - 13.0), vec2(w, 26.0));
            let resp = tool_button(ui, rect, Id::new(("files-tool", &info.id, i)), label, enabled).on_hover_text(tip);
            if resp.clicked() && enabled {
                match label {
                    "Up" => self.go_up(),
                    "New folder" => self.new_folder(ui, info),
                    "Rename" => {
                        if let Some(name) = &selected {
                            self.start_rename(name, ui, info);
                        }
                    }
                    "Copy path" => self.copy_path(ui, selected.as_deref(), info),
                    _ => {
                        if let Some(name) = &selected {
                            self.ask_trash(name, info);
                        }
                    }
                }
            }
            x += w + 4.0;
            used += w + 4.0;
        }
        let rect = Rect::from_min_size(pos2(r.max.x - 8.0 - manager_w, y - 13.0), vec2(manager_w, 26.0));
        if r.width() > manager_w + 60.0
            && tool_button(ui, rect, Id::new(("files-manager", &info.id)), "File manager", true)
                .on_hover_text("Show this folder in the system file manager")
                .clicked()
        {
            self.show_in_manager(info);
        }
    }

    fn show_in_manager(&mut self, info: &Info) {
        if let Err(e) = open_external(&self.dir.join(&self.cwd)) {
            self.message = Some((e, true, info.now));
        }
    }

    fn draw_header(&mut self, ui: &mut Ui, info: &Info, r: Rect, columns: Columns) {
        let painter = ui.painter_at(r);
        super::super::hline(&painter, r.max.y - 0.5, r.min.x, r.max.x);
        let y = r.center().y;
        let font = theme::mono(10.5);
        let mut sort_by = None;
        let name_x = r.min.x + 44.0;
        let heads = [
            ("NAME", Some(Sort::Name), name_x, Align2::LEFT_CENTER),
            ("SIZE", None, columns.size_right(r), Align2::RIGHT_CENTER),
            ("MODIFIED", Some(Sort::Time), columns.time_right(r), Align2::RIGHT_CENTER),
        ];
        for (i, (label, sort, x, align)) in heads.into_iter().enumerate() {
            if (i == 1 && !columns.size) || (i == 2 && !columns.time) {
                continue;
            }
            let painter = ui.painter_at(r);
            let text_r = painter.text(pos2(x, y), align, label, font.clone(), c(theme::TEXT_2));
            let Some(sort) = sort else { continue };
            let active = self.sort == sort;
            let hit = text_r.expand2(vec2(14.0, 6.0));
            let resp = ui.interact(hit, Id::new(("files-sort", &info.id, i)), Sense::click()).on_hover_text(match sort {
                Sort::Name => "Sort by name",
                Sort::Time => "Sort by time, newest first",
            });
            if resp.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
            if active {
                // Down for the normal order (A to Z, newest first), up when reversed.
                let tip = pos2(if align == Align2::LEFT_CENTER { text_r.max.x + 8.0 } else { text_r.min.x - 8.0 }, y);
                let s = 3.5;
                let pts = if self.reverse {
                    vec![tip + vec2(-s, s * 0.6), tip + vec2(s, s * 0.6), tip + vec2(0.0, -s * 0.6)]
                } else {
                    vec![tip + vec2(-s, -s * 0.6), tip + vec2(s, -s * 0.6), tip + vec2(0.0, s * 0.6)]
                };
                ui.painter_at(r).add(Shape::convex_polygon(pts, c(theme::TEXT_2), Stroke::NONE));
            }
            if resp.clicked() {
                sort_by = Some(sort);
            }
        }
        if let Some(sort) = sort_by {
            if self.sort == sort {
                self.reverse = !self.reverse;
            } else {
                self.sort = sort;
                self.reverse = false;
            }
            sort_items(&mut self.items, self.sort, self.reverse);
            self.reveal_selected();
        }
    }

    fn draw_list(&mut self, ui: &mut Ui, info: &Info, r: Rect, columns: Columns, out: &mut Out) {
        let id = list_id(&info.id);
        // The list itself takes the focus, for the keys.
        let list_resp = ui.interact(r, id, Sense::click());
        if list_resp.clicked() {
            list_resp.request_focus();
            self.selected = None;
            self.renaming = None;
        }
        let focused = ui.memory(|m| m.has_focus(id));
        if let Some(e) = &self.error {
            let text = format!("{e} It may have been moved or deleted.");
            super::super::agent::placeholder(ui, r, "This folder is gone", &text);
            let b = Rect::from_center_size(r.center() + vec2(0.0, 64.0), vec2(170.0, 32.0));
            if button(ui, b, "Back to the top", ButtonStyle::Quiet).clicked() {
                self.go(PathBuf::new(), None);
            }
            return;
        }
        if self.items.is_empty() {
            let text = format!("Drop files on the window to hand them over to {}.", info.name);
            super::super::agent::placeholder(ui, r, "This folder is empty", &text);
        }
        let since = self.since(info);
        let marks: Vec<Mark> = self.items.iter().map(|i| self.mark(i, since)).collect();
        let now = SystemTime::now();
        let mut click: Option<String> = None;
        let mut open = None;
        let mut menu: Option<MenuAction> = None;
        let mut rename_done = false;
        let mut rename_cancel = false;

        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(r).layout(egui::Layout::top_down(egui::Align::Min)));
        child.set_clip_rect(r.intersect(ui.clip_rect()));
        child.spacing_mut().item_spacing.y = 0.0;
        let mut area = egui::ScrollArea::vertical().id_salt(("files-scroll", &info.id, &self.cwd)).auto_shrink([false, false]);
        if let Some(offset) = self.scroll_to.take() {
            area = area.vertical_scroll_offset(offset);
        }
        let selected = self.selected.clone();
        let renaming = self.renaming.as_ref().map(|(n, _)| n.clone());
        let mut rename_text = self.renaming.as_ref().map(|(_, t)| t.clone()).unwrap_or_default();
        let shown = area.show_rows(&mut child, ROW_H, self.items.len(), |ui, range| {
            for i in range {
                let item = &self.items[i];
                let (row, resp) = ui.allocate_exact_size(vec2(r.width(), ROW_H), Sense::click());
                let painter = ui.painter().clone();
                let painter = &painter;
                let is_selected = selected.as_deref() == Some(item.name.as_str());
                let bg = row.shrink2(vec2(6.0, 1.0));
                if is_selected {
                    let stroke = if focused { Stroke::new(1.0, ca(theme::NEEDS_YOU, 70)) } else { Stroke::NONE };
                    painter.rect(bg, CornerRadius::same(6), c([0x1A, 0x1F, 0x2A]), stroke, StrokeKind::Inside);
                } else if resp.hovered() {
                    painter.rect_filled(bg, CornerRadius::same(6), c([0x15, 0x19, 0x22]));
                }
                let y = row.center().y;
                match marks[i] {
                    Mark::New => {
                        painter.circle_filled(pos2(row.min.x + 16.0, y), 3.5, c(info.color));
                    }
                    Mark::Inside => {
                        painter.circle_stroke(pos2(row.min.x + 16.0, y), 3.0, Stroke::new(1.4, c(info.color)));
                    }
                    Mark::None => {}
                }
                icon(painter, pos2(row.min.x + 32.0, y), item);
                let name_x = row.min.x + 44.0;
                let name_end = columns.name_end(row);
                if renaming.as_deref() == Some(item.name.as_str()) {
                    let field = Rect::from_min_max(pos2(name_x - 4.0, row.min.y + 3.0), pos2(name_end, row.max.y - 3.0));
                    painter.rect(field, CornerRadius::same(5), c(theme::BG), Stroke::new(1.0, c([0x3A, 0x42, 0x52])), StrokeKind::Inside);
                    let mut field_ui = ui.new_child(egui::UiBuilder::new().max_rect(field.shrink2(vec2(6.0, 3.0))));
                    let edit = egui::TextEdit::singleline(&mut rename_text)
                        .id(rename_id(&info.id))
                        .font(theme::mono(12.5))
                        .frame(egui::Frame::NONE)
                        .margin(egui::Margin::ZERO)
                        .desired_width(field.width() - 12.0);
                    let edit_resp = field_ui.add(edit);
                    if edit_resp.lost_focus() {
                        if ui.input(|i| i.key_pressed(Key::Escape)) {
                            rename_cancel = true;
                        } else {
                            rename_done = true;
                        }
                    }
                } else {
                    let color = if is_selected || marks[i] == Mark::New {
                        c(theme::TEXT)
                    } else if item.name.starts_with('.') {
                        c(FAINT)
                    } else if item.dir {
                        c([0xC9, 0xCE, 0xD9])
                    } else {
                        c(DIM)
                    };
                    let text = fit(painter, &item.name, theme::mono(12.5), name_end - name_x);
                    painter.text(pos2(name_x, y), Align2::LEFT_CENTER, text, theme::mono(12.5), color);
                }
                if columns.size {
                    let size = if item.dir {
                        format!("{} item{}", item.size, if item.size == 1 { "" } else { "s" })
                    } else {
                        tree::human_size(item.size)
                    };
                    painter.text(
                        pos2(columns.size_right(row), y),
                        Align2::RIGHT_CENTER,
                        size,
                        theme::mono(11.5),
                        c(FAINT.map(|v| v + 0x20)),
                    );
                }
                if columns.time {
                    let t = item.modified.map(|m| when(m, now)).unwrap_or_default();
                    painter.text(pos2(columns.time_right(row), y), Align2::RIGHT_CENTER, t, theme::mono(11.5), c(DIM));
                }
                if resp.clicked() || resp.secondary_clicked() {
                    click = Some(item.name.clone());
                }
                if resp.double_clicked() {
                    open = Some(item.name.clone());
                }
                let name = item.name.clone();
                let dir = item.dir;
                resp.context_menu(|ui| {
                    ui.set_min_width(190.0);
                    if ui.button(if dir { "Open folder" } else { "Open" }).clicked() {
                        menu = Some(MenuAction::Open(name.clone()));
                        ui.close();
                    }
                    if ui.button("Rename").clicked() {
                        menu = Some(MenuAction::Rename(name.clone()));
                        ui.close();
                    }
                    if ui.button("Copy path").clicked() {
                        menu = Some(MenuAction::CopyPath(Some(name.clone())));
                        ui.close();
                    }
                    if ui.button("Show in the system file manager").clicked() {
                        menu = Some(MenuAction::Manager);
                        ui.close();
                    }
                    ui.separator();
                    let trash = ui.add_enabled(has_gio(), egui::Button::new("Move to trash"));
                    let trash = if has_gio() { trash } else { trash.on_disabled_hover_text(NO_GIO) };
                    if trash.clicked() {
                        menu = Some(MenuAction::Trash(name.clone()));
                        ui.close();
                    }
                });
            }
        });
        self.offset = shown.state.offset.y;
        self.view_h = shown.inner_rect.height();
        if let Some((_, text)) = &mut self.renaming {
            *text = rename_text;
        }
        // Clicks below the rows still belong to the empty part of the list.
        list_resp.context_menu(|ui| {
            ui.set_min_width(190.0);
            if ui.button("New folder").clicked() {
                menu = Some(MenuAction::NewFolder);
                ui.close();
            }
            if ui.button("Copy path of this folder").clicked() {
                menu = Some(MenuAction::CopyPath(None));
                ui.close();
            }
            if ui.button("Show in the system file manager").clicked() {
                menu = Some(MenuAction::Manager);
                ui.close();
            }
        });
        if let Some(name) = click {
            if self.renaming.as_ref().is_some_and(|(n, _)| *n != name) {
                self.renaming = None;
            }
            self.selected = Some(name);
            ui.memory_mut(|m| m.request_focus(id));
        }
        if rename_cancel || rename_done {
            // The Enter or Esc that ended the rename is not for the list.
            ui.input_mut(|i| {
                i.consume_key(Modifiers::NONE, Key::Enter);
                i.consume_key(Modifiers::NONE, Key::Escape);
            });
        }
        if rename_cancel {
            self.renaming = None;
            ui.memory_mut(|m| m.request_focus(id));
        } else if rename_done {
            self.finish_rename(info);
            ui.memory_mut(|m| m.request_focus(id));
        }
        if let Some(name) = open {
            self.activate(&name, out);
        }
        match menu {
            Some(MenuAction::Open(name)) => self.activate(&name, out),
            Some(MenuAction::Rename(name)) => {
                self.selected = Some(name.clone());
                self.start_rename(&name, ui, info);
            }
            Some(MenuAction::CopyPath(name)) => self.copy_path(ui, name.as_deref(), info),
            Some(MenuAction::Manager) => self.show_in_manager(info),
            Some(MenuAction::Trash(name)) => {
                self.selected = Some(name.clone());
                self.ask_trash(&name, info);
            }
            Some(MenuAction::NewFolder) => self.new_folder(ui, info),
            None => {}
        }
    }

    fn draw_confirm(&mut self, ui: &mut Ui, r: Rect) {
        let Some(name) = self.confirm.clone() else { return };
        let painter = ui.painter_at(r);
        painter.rect_filled(r, CornerRadius::ZERO, ca(DANGER, 18));
        super::super::hline(&painter, r.min.y + 0.5, r.min.x, r.max.x);
        let dir = self.items.iter().any(|i| i.name == name && i.dir);
        let question =
            if dir { format!("Move the folder {name} and all in it to the trash?") } else { format!("Move {name} to the trash?") };
        let y = r.center().y;
        let yes = Rect::from_min_size(pos2(r.max.x - 12.0 - 72.0 - 8.0 - 118.0, y - 15.0), vec2(118.0, 30.0));
        let no = Rect::from_min_size(pos2(r.max.x - 12.0 - 72.0, y - 15.0), vec2(72.0, 30.0));
        let text = fit(&painter, &question, theme::ui(13.5), yes.min.x - r.min.x - 24.0);
        painter.text(pos2(r.min.x + 14.0, y), Align2::LEFT_CENTER, text, theme::ui(13.5), c(theme::TEXT));
        if button(ui, yes, "Move to trash", ButtonStyle::Primary).clicked() {
            self.confirm = None;
            self.trash(&name);
        }
        if button(ui, no, "Cancel", ButtonStyle::Quiet).clicked() {
            self.confirm = None;
        }
    }

    fn draw_status(&mut self, ui: &mut Ui, info: &Info, r: Rect) {
        let painter = ui.painter_at(r);
        let corners = CornerRadius { nw: 0, ne: 0, sw: 13, se: 13 };
        painter.rect_filled(r, corners, c(BAR_BG));
        super::super::hline(&painter, r.min.y + 0.5, r.min.x, r.max.x);
        let font = theme::mono(11.0);
        let y = r.center().y;
        let mut x = r.min.x + 12.0;
        let n = self.items.len();
        let count = format!("{n} item{}", if n == 1 { "" } else { "s" });
        x = painter.text(pos2(x, y), Align2::LEFT_CENTER, count, font.clone(), c(DIM)).max.x + 14.0;
        let since = self.since(info);
        let new = self.items.iter().filter(|i| self.mark(i, since) == Mark::New).count();
        if new > 0 {
            painter.circle_filled(pos2(x + 3.5, y), 3.5, c(info.color));
            let text = format!("{new} new from {}", info.name);
            x = painter.text(pos2(x + 12.0, y), Align2::LEFT_CENTER, text, font.clone(), c(info.color)).max.x + 14.0;
        }
        let note = match &self.message {
            Some((text, error, at)) if info.now - at < 6.0 => Some((text.clone(), *error)),
            _ if info.external => Some(("Files here stay on your computer. External agents do not see them.".to_string(), false)),
            _ => None,
        };
        if let Some((text, error)) = note {
            let room = r.max.x - 12.0 - x;
            if room > 60.0 {
                let shown = fit(&painter, &text, font.clone(), room);
                painter.text(pos2(x, y), Align2::LEFT_CENTER, shown, font, if error { c(DANGER) } else { c(theme::TEXT_2) });
            }
        }
    }
}

enum MenuAction {
    Open(String),
    Rename(String),
    CopyPath(Option<String>),
    Manager,
    Trash(String),
    NewFolder,
}

/// Start of the agent's last run from its `state.json`, written by the daemon.
fn last_run_start(dir: &Path) -> Option<SystemTime> {
    let text = std::fs::read_to_string(dir.join("state.json")).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    let ms = value.get("last_run")?.get("started")?.as_u64()?;
    Some(SystemTime::UNIX_EPOCH + Duration::from_millis(ms))
}

fn shown_path(rel: &Path) -> String {
    if rel.as_os_str().is_empty() { "the agent's folder".to_string() } else { format!("{}/", rel.display()) }
}

/// Which columns fit the tile.
#[derive(Clone, Copy)]
struct Columns {
    size: bool,
    time: bool,
}

impl Columns {
    const SIZE_W: f32 = 86.0;
    const TIME_W: f32 = 124.0;

    fn for_width(w: f32) -> Self {
        Self { size: w >= 470.0, time: w >= 330.0 }
    }

    fn time_right(self, r: Rect) -> f32 {
        r.max.x - 18.0
    }

    fn size_right(self, r: Rect) -> f32 {
        self.time_right(r) - if self.time { Self::TIME_W } else { 0.0 }
    }

    fn name_end(self, r: Rect) -> f32 {
        self.size_right(r) - if self.size { Self::SIZE_W } else { 0.0 } - 10.0
    }
}

/// A small button for the tool bar, greyed out when disabled.
fn tool_button(ui: &mut Ui, rect: Rect, id: Id, label: &str, enabled: bool) -> egui::Response {
    let resp = ui.interact(rect, id, if enabled { Sense::click() } else { Sense::hover() });
    let hovered = resp.hovered() && enabled;
    let painter = ui.painter();
    let fill = if hovered { c([0x1A, 0x1F, 0x2A]) } else { Color32::TRANSPARENT };
    let stroke = Stroke::new(1.0, if hovered { c([0x3A, 0x42, 0x52]) } else { c([0x24, 0x29, 0x34]) });
    painter.rect(rect, CornerRadius::same(7), fill, stroke, StrokeKind::Inside);
    let color = if enabled { c([0xC9, 0xCE, 0xD9]) } else { c(FAINT) };
    painter.text(rect.center(), Align2::CENTER_CENTER, label, theme::ui(13.0), color);
    if hovered {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp
}

/// A folder or a page with a fold, tinted by what the file is.
fn icon(painter: &egui::Painter, center: egui::Pos2, item: &Item) {
    if item.dir {
        let body = Rect::from_center_size(center + vec2(0.0, 1.0), vec2(15.0, 10.5));
        let tab = Rect::from_min_size(body.min - vec2(0.0, 2.5), vec2(6.5, 3.5));
        painter.rect_filled(tab, CornerRadius::same(1), c([0x6E, 0x7A, 0x90]));
        painter.rect_filled(body, CornerRadius::same(2), c([0x6E, 0x7A, 0x90]));
        return;
    }
    let accent = match viewer::kind_by_name(Path::new(&item.name)) {
        Some(Kind::Image) => [0x8C, 0xD3, 0xFF],
        Some(Kind::Markdown) => [0xB7, 0xA6, 0xFF],
        Some(Kind::Csv) => [0x8F, 0xD1, 0x9E],
        Some(Kind::Html) => [0xF2, 0xB4, 0x5A],
        Some(Kind::Pdf) => [0xE0, 0x67, 0x5A],
        _ => [0x6B, 0x73, 0x85],
    };
    let page = Rect::from_center_size(center, vec2(11.0, 14.0));
    let fold = 3.5;
    let stroke = Stroke::new(1.2, c(accent));
    let pts = vec![
        page.left_top(),
        pos2(page.max.x - fold, page.min.y),
        pos2(page.max.x, page.min.y + fold),
        page.right_bottom(),
        page.left_bottom(),
        page.left_top(),
    ];
    painter.add(Shape::line(pts, stroke));
    painter.line_segment([pos2(page.max.x - fold, page.min.y), pos2(page.max.x - fold, page.min.y + fold)], stroke);
    painter.line_segment([pos2(page.max.x - fold, page.min.y + fold), pos2(page.max.x, page.min.y + fold)], stroke);
}

// Agent and app standard (D30, agent_apps.rs).

/// Files instance `n` of `agent`, made when it was never shown.
fn agent_tile<'a>(core: &'a mut Core, agent: &str, n: u32) -> Option<&'a mut FilesTile> {
    let dir = core.agent_dir(agent)?.path.clone();
    let t = core.ui.files.agents.entry((agent.to_string(), n)).or_insert_with(|| FilesTile::new(dir.clone()));
    if t.dir != dir {
        *t = FilesTile::new(dir);
    }
    Some(t)
}

/// The folder Files instance `n` of `agent` shows, relative to the agent
/// directory, with its entries sorted by name, folders first.
pub(crate) fn agent_listing(core: &mut Core, agent: &str, n: u32) -> Option<(PathBuf, Listing)> {
    let t = agent_tile(core, agent, n)?;
    let mut listing = list(&t.dir, &t.cwd);
    if let Ok(items) = &mut listing {
        sort_items(items, Sort::Name, false);
    }
    Some((t.cwd.clone(), listing))
}

/// Shows folder `rel` in Files instance `n` of `agent`.
pub(crate) fn agent_go(core: &mut Core, agent: &str, n: u32, rel: &Path) {
    if let Some(t) = agent_tile(core, agent, n) {
        t.go(rel.to_path_buf(), None);
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::fs;

    use super::*;

    /// A directory under the system temp dir, removed again on drop.
    pub struct TempDir(pub PathBuf);

    impl TempDir {
        pub fn new(name: &str) -> Self {
            static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let nanos = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().subsec_nanos();
            let dir = std::env::temp_dir().join(format!("meerora-{name}-{}-{nanos}-{n}", std::process::id()));
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        pub fn write(&self, rel: &str, bytes: &[u8]) -> PathBuf {
            let path = self.0.join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, bytes).unwrap();
            path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn touch(path: &Path, minutes_ago: u64) {
        let t = SystemTime::now() - Duration::from_secs(60 * minutes_ago);
        fs::File::options().write(true).open(path).unwrap().set_modified(t).unwrap();
    }

    fn names(items: &[Item]) -> Vec<String> {
        items.iter().map(|i| format!("{}{}", i.name, if i.dir { "/" } else { "" })).collect()
    }

    #[test]
    fn a_folder_lists_folders_first_and_hides_git_and_logs() {
        let tmp = TempDir::new("files-list");
        for rel in [".git/HEAD", "logs/run.log", "memory/logs/kept.md", "memory/.git/x", "artifacts/report.md"] {
            tmp.write(rel, b"x");
        }
        let b = tmp.write("B.md", b"hello");
        let a = tmp.write("a.txt", b"x");
        let env = tmp.write(".env", b"x");
        let plot = tmp.write("plot.png", b"x");
        touch(&b, 30);
        touch(&a, 5);
        touch(&env, 60);
        touch(&plot, 1);

        let mut items = list(&tmp.0, Path::new("")).unwrap();
        sort_items(&mut items, Sort::Name, false);
        assert_eq!(names(&items), ["artifacts/", "memory/", ".env", "a.txt", "B.md", "plot.png"], ".git and logs are hidden");
        assert_eq!(items.iter().find(|i| i.name == "B.md").unwrap().size, 5);
        assert_eq!(items.iter().find(|i| i.name == "memory").unwrap().size, 1, "memory/.git does not count");

        sort_items(&mut items, Sort::Name, true);
        assert_eq!(names(&items), ["memory/", "artifacts/", "plot.png", "B.md", "a.txt", ".env"], "reversed, folders stay first");

        sort_items(&mut items, Sort::Time, false);
        let files: Vec<String> = names(&items).into_iter().filter(|n| !n.ends_with('/')).collect();
        assert_eq!(files, ["plot.png", "a.txt", "B.md", ".env"], "newest first");
        sort_items(&mut items, Sort::Time, true);
        assert_eq!(names(&items).last().unwrap(), "plot.png");

        // Below the top, logs is an ordinary folder.
        let inner = list(&tmp.0, Path::new("memory")).unwrap();
        assert_eq!(names(&inner), ["logs/"]);
        assert!(list(&tmp.0, Path::new("missing")).is_err());
    }

    #[test]
    fn handed_over_files_get_a_free_name_and_never_overwrite() {
        let tmp = TempDir::new("files-hand-over");
        let into = tmp.0.join("artifacts");
        fs::create_dir_all(&into).unwrap();
        fs::write(into.join("report.pdf"), b"agent's").unwrap();
        fs::write(into.join(".env"), b"x").unwrap();
        fs::write(into.join("notes"), b"x").unwrap();
        assert_eq!(unique_name(&into, "new.pdf"), "new.pdf");
        assert_eq!(unique_name(&into, "report.pdf"), "report (2).pdf");
        assert_eq!(unique_name(&into, ".env"), ".env (2)");
        assert_eq!(unique_name(&into, "notes"), "notes (2)");
        fs::write(into.join("report (2).pdf"), b"x").unwrap();
        assert_eq!(unique_name(&into, "report.pdf"), "report (3).pdf");
        assert_eq!(unique_name(&into, "report (2).pdf"), "report (3).pdf", "the number counts up");

        let outside = TempDir::new("files-hand-over-src");
        let src = outside.write("report.pdf", b"yours");
        let folder = Folder::at(&into).unwrap();
        assert_eq!(copy_into(&src, &folder).unwrap(), "report (3).pdf");
        assert_eq!(fs::read(into.join("report.pdf")).unwrap(), b"agent's", "nothing is overwritten");
        assert_eq!(fs::read(into.join("report (3).pdf")).unwrap(), b"yours");

        outside.write("data/rows.csv", b"a,b\n");
        outside.write("data/deep/x.txt", b"x");
        assert_eq!(copy_into(&outside.0.join("data"), &folder).unwrap(), "data");
        assert_eq!(copy_into(&outside.0.join("data"), &folder).unwrap(), "data (2)");
        assert_eq!(fs::read(into.join("data (2)/deep/x.txt")).unwrap(), b"x");
        assert!(copy_into(&tmp.0, &folder).is_err(), "a folder cannot go into itself");
    }

    /// Runs the hand-over of `paths` into `cwd` and waits for its result.
    fn hand_over_into(t: &mut FilesTile, cwd: &str, paths: Vec<PathBuf>) -> (Vec<String>, Vec<String>) {
        t.cwd = PathBuf::from(cwd);
        t.hand_over(paths);
        match t.jobs.pop().unwrap().recv_timeout(Duration::from_secs(10)).unwrap() {
            Done::HandedOver { names, failed, .. } => (names, failed),
            Done::Trashed { .. } => panic!("not a hand-over"),
        }
    }

    #[test]
    fn rename_and_hand_over_never_follow_a_link_out_of_the_agent_directory() {
        let tmp = TempDir::new("files-links");
        let yours = TempDir::new("files-links-yours");
        yours.write("id_ed25519", b"secret");
        let dropped = TempDir::new("files-links-dropped");
        let file = dropped.write("note.txt", b"hello");
        dropped.write("folder/inner.txt", b"x");
        let info = Info { id: "ember".into(), name: "Ember".into(), color: [0xC7, 0x78, 0x4A], now: 1.0, run: None, external: false };

        // The agent swapped a folder you had open for a link to one of yours.
        tmp.write("inbox/keep.txt", b"agent's");
        fs::remove_dir_all(tmp.0.join("inbox")).unwrap();
        std::os::unix::fs::symlink(&yours.0, tmp.0.join("inbox")).unwrap();
        let mut t = FilesTile::new(tmp.0.clone());
        let (names, failed) = hand_over_into(&mut t, "inbox", vec![file.clone()]);
        assert!(names.is_empty() && failed.len() == 1, "{names:?} {failed:?}");
        assert!(!yours.0.join("note.txt").exists(), "nothing landed in your folder");

        t.cwd = PathBuf::from("inbox");
        t.renaming = Some(("id_ed25519".into(), "stolen".into()));
        t.finish_rename(&info);
        assert!(yours.0.join("id_ed25519").exists() && !yours.0.join("stolen").exists(), "your file keeps its name");
        assert!(t.message.as_ref().is_some_and(|(m, error, _)| *error && m.contains("link")), "{:?}", t.message);
        t.cwd = PathBuf::from("inbox");
        let ctx = egui::Context::default();
        ctx.run_ui(egui::RawInput::default(), |ui| t.new_folder(ui, &info)).textures_delta.clear();
        assert!(!yours.0.join("New folder").exists());

        // A link deeper down, below a real folder, is refused the same way.
        fs::create_dir_all(tmp.0.join("real")).unwrap();
        std::os::unix::fs::symlink(&yours.0, tmp.0.join("real/deep")).unwrap();
        let (names, _) = hand_over_into(&mut t, "real/deep", vec![file.clone()]);
        assert!(names.is_empty() && !yours.0.join("note.txt").exists());

        // Real folders still work, folders included.
        let (names, failed) = hand_over_into(&mut t, "real", vec![file, dropped.0.join("folder")]);
        assert_eq!((names.as_slice(), failed.len()), (&["note.txt".to_string(), "folder".to_string()][..], 0));
        assert_eq!(fs::read(tmp.0.join("real/folder/inner.txt")).unwrap(), b"x");
    }

    #[test]
    fn action_summaries_name_the_agents_files() {
        let tmp = TempDir::new("files-named");
        tmp.write("run.py", b"x");
        tmp.write("memory/notes.md", b"x");
        tmp.write("my notes.md", b"x");
        assert_eq!(named_files(&tmp.0, "run.py +5 -1"), [PathBuf::from("run.py")]);
        assert_eq!(named_files(&tmp.0, "run.py, memory/notes.md"), [PathBuf::from("run.py"), PathBuf::from("memory/notes.md")]);
        assert_eq!(named_files(&tmp.0, "my notes.md +3 -1"), [PathBuf::from("my notes.md")]);
        assert!(named_files(&tmp.0, "missing.txt").is_empty());
        assert!(named_files(&tmp.0, "../etc/passwd").is_empty());
    }

    #[test]
    fn times_read_short() {
        let now = SystemTime::now();
        assert_eq!(when(now, now), "just now");
        assert_eq!(when(now - Duration::from_secs(12 * 60 + 5), now), "12 min ago");
        assert!(when(now - Duration::from_secs(400 * 86_400), now).len() >= "1 Jan 2025".len() - 1, "a year shows");
    }

    #[test]
    fn the_agents_latest_files_are_marked() {
        let tmp = TempDir::new("files-marks");
        let old = tmp.write("old.md", b"x");
        let new = tmp.write("artifacts/plot.png", b"x");
        tmp.write("run.py", b"x");
        touch(&old, 120);
        touch(&new, 1);
        let started = SystemTime::now() - Duration::from_secs(600);
        let ms = started.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_millis();
        tmp.write("state.json", format!("{{\"last_run\":{{\"started\":{ms}}}}}").as_bytes());
        let mut t = FilesTile::new(tmp.0.clone());
        let info = Info { id: "ember".into(), name: "Ember".into(), color: [1, 2, 3], now: 1.0, run: None, external: false };
        let patch = ActionEntry { at: 0.5, tool: "editor.patch".into(), summary: "run.py +5 -1".into(), target: None };
        t.track(&info, &[patch]);
        let since = t.since(&info);
        assert!(since.is_some(), "from state.json");
        let mark = |t: &FilesTile, name: &str| t.mark(t.items.iter().find(|i| i.name == name).unwrap(), since);
        assert_eq!(mark(&t, "run.py"), Mark::New, "patched, from the action log");
        assert_eq!(mark(&t, "artifacts"), Mark::Inside, "holds a file changed since the run began");
        assert_eq!(mark(&t, "old.md"), Mark::None);
        assert_eq!(mark(&t, "state.json"), Mark::None, "the daemon's file");
        t.yours.insert(PathBuf::from("run.py"));
        assert_eq!(mark(&t, "run.py"), Mark::None, "what you put here is yours");
    }

    #[test]
    fn patched_files_come_from_the_full_target() {
        let tmp = TempDir::new("files-target");
        let deep = format!("{}/notes.md", "nested-folder".repeat(8));
        tmp.write(&deep, b"x");
        let mut t = FilesTile::new(tmp.0.clone());
        let info = Info { id: "ember".into(), name: "Ember".into(), color: [1, 2, 3], now: 1.0, run: None, external: false };
        let cut: String = deep.chars().take(79).chain(['…']).collect();
        t.track(&info, &[ActionEntry { at: 0.5, tool: "editor.patch".into(), summary: cut, target: Some(deep.clone()) }]);
        assert!(t.recent.contains(&PathBuf::from(&deep)), "the cut summary names no file: {:?}", t.recent);
    }

    #[test]
    fn the_tile_draws_without_a_window() {
        let tmp = TempDir::new("files-draw");
        tmp.write("artifacts/report.md", b"# Report");
        tmp.write("run.py", b"print(1)");
        let mut t = FilesTile::new(tmp.0.clone());
        let info = Info { id: "ember".into(), name: "Ember".into(), color: [0xC7, 0x78, 0x4A], now: 1.0, run: None, external: false };
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        let screen = Rect::from_min_size(egui::Pos2::ZERO, vec2(900.0, 600.0));
        for width in [800.0, 300.0] {
            let raw = egui::RawInput { screen_rect: Some(screen), ..Default::default() };
            let mut output = ctx.run_ui(raw, |ui| {
                let head = Rect::from_min_size(pos2(10.0, 10.0), vec2(width, 38.0));
                let body = Rect::from_min_max(pos2(11.0, 49.0), pos2(10.0 + width, 590.0));
                let out = t.show(ui, &info, &[], head, body);
                assert!(!out.focused);
            });
            output.textures_delta.clear();
        }
        assert_eq!(names(&t.items), ["artifacts/", "run.py"]);
        t.selected = Some("artifacts".into());
        let mut out = Out::default();
        t.activate("artifacts", &mut out);
        assert_eq!(t.cwd, PathBuf::from("artifacts"));
        t.activate("report.md", &mut out);
        assert_eq!(out.open, Some(PathBuf::from("artifacts/report.md")));
        t.go_up();
        assert_eq!(t.cwd, PathBuf::new());
        assert_eq!(t.selected.as_deref(), Some("artifacts"), "going up selects the folder you came from");
    }
}
