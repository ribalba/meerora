//! The Viewer app of the agent view (D11, D21): one result of the agent at
//! a time, chosen in Files, or else the newest file in its `artifacts/`
//! folder. Images fit the tile and zoom and pan, Markdown renders with
//! headings, lists, code, links and tables, CSV shows as a table and other
//! text in a monospace view. HTML and PDF open in the browser app or the
//! system viewer.
//!
//! The shown file is followed live. When the agent writes a new result, the
//! head says "New from <agent>" with a button to show it. The Viewer never
//! switches files by itself while it shows one.

use std::collections::HashMap;
use std::ops::Range;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver};
use std::time::SystemTime;

use egui::text::{LayoutJob, TextFormat};
use egui::{Align2, Color32, CornerRadius, Galley, Id, Rect, Sense, Stroke, StrokeKind, Ui, Vec2, pos2, vec2};

use super::files;
use crate::engine::Core;
use crate::theme::{self, c, ca};
use crate::ui::editor::files as disk;
use crate::ui::{AppTab, ButtonStyle, button, fit, pill};
use crate::world::ActionEntry;

/// Seconds between checks of the shown file on disk.
const CHECK_EVERY: f64 = 1.0;
/// Seconds between scans of `artifacts/` for new results.
const SCAN_EVERY: f64 = 2.0;
const ARTIFACTS: &str = "artifacts";
/// Files `artifacts/` lists at most, and how deep it looks.
const MAX_ARTIFACTS: usize = 2000;
const MAX_DEPTH: usize = 4;
const MAX_MARKDOWN_BYTES: u64 = 4 * 1024 * 1024;
const MAX_CSV_BYTES: u64 = 32 * 1024 * 1024;
const MAX_IMAGE_BYTES: u64 = 64 * 1024 * 1024;
/// Rows of a CSV file the table shows at most.
pub const MAX_ROWS: usize = 20_000;
/// Agents write these files, so every view has limits that keep a hostile
/// file from stalling the window: columns and characters of a CSV field,
/// rows, columns and blocks of a Markdown document, the size of a text file
/// and the characters of one of its lines.
pub const MAX_COLUMNS: usize = 256;
pub const MAX_FIELD_CHARS: usize = 2000;
pub const MAX_TABLE_ROWS: usize = 500;
pub const MAX_TABLE_COLUMNS: usize = 50;
pub const MAX_BLOCKS: usize = 10_000;
const MAX_TEXT_BYTES: u64 = 32 * 1024 * 1024;
pub const MAX_LINE_CHARS: usize = 10_000;
/// Characters of a CSV cell that are laid out to fit it, far more than a column shows.
const CELL_CHARS: usize = 120;
/// The largest texture side the Viewer uploads, whatever the GPU allows.
const MAX_SIDE: usize = 4096;

const DIM: [u8; 3] = [0x8A, 0x92, 0xA3];
const LINE_NO: [u8; 3] = [0x4A, 0x51, 0x60];
const BODY_TEXT: [u8; 3] = [0xD5, 0xD9, 0xE2];
const TEXT_LINE_H: f32 = 19.0;
const CSV_ROW_H: f32 = 26.0;

/// What a file is, for the Viewer and for routing (see [`route`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Image,
    Markdown,
    Csv,
    Text,
    Html,
    Pdf,
    /// Anything else that is not text. Only the system viewer opens it.
    Binary,
}

impl Kind {
    fn label(self) -> &'static str {
        match self {
            Kind::Image => "IMAGE",
            Kind::Markdown => "MARKDOWN",
            Kind::Csv => "CSV",
            Kind::Text => "TEXT",
            Kind::Html => "HTML",
            Kind::Pdf => "PDF",
            Kind::Binary => "FILE",
        }
    }

    /// Text that the editor can open as well.
    fn is_text(self) -> bool {
        matches!(self, Kind::Markdown | Kind::Csv | Kind::Text | Kind::Html)
    }
}

/// The kind a file name tells, without reading the file.
pub fn kind_by_name(path: &Path) -> Option<Kind> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "webp" => Kind::Image,
        "md" | "markdown" => Kind::Markdown,
        "csv" | "tsv" => Kind::Csv,
        "html" | "htm" | "svg" => Kind::Html,
        "pdf" => Kind::Pdf,
        _ => return None,
    })
}

/// The kind of a file: by its name, else text or binary by its first bytes.
pub fn kind_of(path: &Path) -> Kind {
    if let Some(kind) = kind_by_name(path) {
        return kind;
    }
    let mut head = Vec::with_capacity(8000);
    let read = std::fs::File::open(path).and_then(|f| std::io::Read::read_to_end(&mut std::io::Read::take(f, 8000), &mut head));
    if read.is_ok() && disk::is_binary(&head) { Kind::Binary } else { Kind::Text }
}

/// Which app opens a file from Files.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    Editor,
    Viewer(Kind),
}

/// Text and code open in the editor. Images, Markdown, CSV, HTML, PDF and
/// other binary files open in the Viewer.
pub fn route(path: &Path) -> Route {
    match kind_of(path) {
        Kind::Text => Route::Editor,
        kind => Route::Viewer(kind),
    }
}

/// The Viewer state of every Viewer instance that was used, by agent and
/// instance number. Each instance shows its own file.
#[derive(Default)]
pub struct Viewers {
    agents: HashMap<(String, u32), ViewerTile>,
}

/// Shows `rel`, a path inside the agent's directory, in the agent's Viewer
/// and opens the Viewer app. Files uses this. With several Viewers it is
/// the one you used last (`agent::target`).
pub fn show_file(core: &mut Core, agent: &str, rel: &Path) {
    let Some(dir) = core.agent_dir(agent).map(|d| d.path.clone()) else { return };
    let tab = super::super::agent::target(core, agent, meerora_protocol::App::Viewer);
    state(core, agent, tab.instance(), dir).choose(rel.to_path_buf());
    super::super::agent::open_later(core, tab);
}

/// Files and folders you put into the agent's directory yourself, so they
/// never show as new from the agent, in every Viewer of the agent.
pub fn mark_yours(core: &mut Core, agent: &str, rels: &[PathBuf]) {
    for ((a, _), t) in core.ui.viewer.agents.iter_mut() {
        if a != agent {
            continue;
        }
        let Some(known) = &mut t.known else { continue };
        for rel in rels {
            let path = t.dir.join(rel);
            if path.is_dir() {
                let mut found = Vec::new();
                walk(&t.dir, rel, 0, &mut found);
                known.extend(found);
            } else if let Some((modified, _)) = disk::stamp(&path) {
                known.insert(rel.clone(), modified);
            }
        }
    }
}

fn state<'a>(core: &'a mut Core, agent: &str, n: u32, dir: PathBuf) -> &'a mut ViewerTile {
    let t = core.ui.viewer.agents.entry((agent.to_string(), n)).or_insert_with(|| ViewerTile::new(dir.clone()));
    if t.dir != dir {
        *t = ViewerTile::new(dir);
    }
    t
}

/// Draws Viewer instance `n` of agent `index`. The Viewer takes no keyboard
/// input, so it never holds the focus.
pub fn tile(ui: &mut Ui, core: &mut Core, index: usize, n: u32, head: Rect, body: Rect) -> bool {
    let a = &core.world.agents[index];
    let id = a.id.clone();
    let info = Info { id: crate::ui::instance_key(&id, n), name: a.name.clone(), now: core.world.time };
    let actions: Vec<ActionEntry> = a.actions.iter().filter(|e| e.tool == "editor.patch").cloned().collect();
    let Some(dir) = core.agent_dir(&id).map(|d| d.path.clone()) else {
        super::super::agent::placeholder(ui, body, "No directory", "This agent has no directory to show results from.");
        return false;
    };
    let key = (id.clone(), n);
    let mut t = core.ui.viewer.agents.remove(&key).filter(|t| t.dir == dir).unwrap_or_else(|| ViewerTile::new(dir));
    let out = t.show(ui, &info, &actions, head, body);
    core.ui.viewer.agents.insert(key, t);
    match out.action {
        Some(Action::Editor(rel)) => crate::ui::editor::open_file(core, &id, &rel),
        Some(Action::Browser(url)) => crate::ui::browser::open_url(core, &id, &url),
        Some(Action::Open(rel)) => files::open_path(core, &id, &rel),
        None => {}
    }
    if let Some((text, error)) = out.toast {
        core.ui.toast(info.now, text, error);
    }
    if out.clicked {
        core.ui.tab = AppTab::Viewer(n);
    }
    false
}

struct Info {
    /// The instance key for egui ids and the texture name (`ui::instance_key`).
    id: String,
    name: String,
    now: f64,
}

enum Action {
    Editor(PathBuf),
    Browser(String),
    /// A file a Markdown link points at, opened like a double click in Files.
    Open(PathBuf),
}

#[derive(Default)]
struct Out {
    clicked: bool,
    action: Option<Action>,
    toast: Option<(String, bool)>,
    /// A link inside the shown document was clicked.
    link: Option<String>,
}

/// Zoom and pan of an image. `zoom` is `None` while the image fits the tile.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct ImageView {
    zoom: Option<f32>,
    pan: Vec2,
}

struct ViewerTile {
    dir: PathBuf,
    /// The shown file, relative to the agent directory.
    shown: Option<PathBuf>,
    doc: Option<Doc>,
    loading: Option<(PathBuf, Receiver<Loaded>)>,
    checked_at: f64,
    scanned_at: f64,
    /// The files in `artifacts/` with their modification times at the last scan.
    known: Option<HashMap<PathBuf, SystemTime>>,
    /// A result the agent wrote that is not shown yet.
    fresh: Option<PathBuf>,
    seen_action: Option<f64>,
    view: ImageView,
    max_side: usize,
}

struct Doc {
    rel: PathBuf,
    kind: Kind,
    stamp: Option<(SystemTime, u64)>,
    body: Body,
}

enum Body {
    Image {
        texture: egui::TextureHandle,
        size: [usize; 2],
        note: Option<String>,
    },
    Markdown(Vec<Block>),
    Csv {
        rows: Vec<Vec<String>>,
        note: Option<String>,
        widths: Option<Vec<f32>>,
    },
    Text {
        lines: Vec<String>,
        note: Option<String>,
        widest: usize,
    },
    /// HTML and PDF, shown with the buttons that open them elsewhere.
    Link,
    Binary,
    Error(String),
}

/// A file as the loader thread read it.
pub struct Loaded {
    kind: Kind,
    stamp: Option<(SystemTime, u64)>,
    content: Content,
}

enum Content {
    Image { image: egui::ColorImage, size: [usize; 2], note: Option<String> },
    Markdown(Vec<Block>),
    Csv { rows: Vec<Vec<String>>, note: Option<String> },
    Text { lines: Vec<String>, note: Option<String> },
    Link,
    Binary,
    Error(String),
}

/// Reads and decodes a file for the Viewer. Images larger than `max_side`
/// are scaled down.
pub fn load(path: &Path, max_side: usize) -> Loaded {
    // The stamp comes first, so a write during the read shows as a change.
    let stamp = disk::stamp(path);
    let kind = kind_of(path);
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let Some((_, size)) = stamp else {
        return Loaded { kind, stamp, content: Content::Error(format!("{name} is gone. It was moved or deleted.")) };
    };
    let too_big = |limit: u64| {
        Content::Error(format!("{name} is {}. The Viewer shows files up to {}.", disk::human_size(size), disk::human_size(limit)))
    };
    let content = match kind {
        Kind::Image if size > MAX_IMAGE_BYTES => too_big(MAX_IMAGE_BYTES),
        Kind::Image => load_image(path, max_side),
        Kind::Markdown if size > MAX_MARKDOWN_BYTES => too_big(MAX_MARKDOWN_BYTES),
        Kind::Markdown => match std::fs::read(path) {
            Ok(bytes) => Content::Markdown(markdown_blocks(&String::from_utf8_lossy(&bytes))),
            Err(e) => Content::Error(format!("Cannot read {name} ({e}).")),
        },
        Kind::Csv if size > MAX_CSV_BYTES => too_big(MAX_CSV_BYTES),
        Kind::Csv => match std::fs::read(path) {
            Ok(bytes) => {
                let text = String::from_utf8_lossy(&bytes);
                let tsv = path.extension().is_some_and(|e| e.eq_ignore_ascii_case("tsv"));
                let mut rows = parse_csv(&text, if tsv { '\t' } else { sniff_separator(&text) }, MAX_ROWS + 1);
                let note = (rows.len() > MAX_ROWS).then(|| format!("Showing the first {MAX_ROWS} rows."));
                rows.truncate(MAX_ROWS);
                Content::Csv { rows, note }
            }
            Err(e) => Content::Error(format!("Cannot read {name} ({e}).")),
        },
        Kind::Text if size > MAX_TEXT_BYTES => too_big(MAX_TEXT_BYTES),
        Kind::Text => match disk::load(path) {
            disk::Loaded::Text { text, read_only, .. } => Content::Text { lines: text.lines().map(shown_line).collect(), note: read_only },
            disk::Loaded::Binary(note) | disk::Loaded::Error(note) => Content::Error(note),
        },
        Kind::Html | Kind::Pdf => Content::Link,
        Kind::Binary => Content::Binary,
    };
    Loaded { kind, stamp, content }
}

/// A line of a text file as the Viewer draws it: tabs as spaces, cut after
/// [`MAX_LINE_CHARS`] characters.
fn shown_line(line: &str) -> String {
    let mut out: String = line.chars().take(MAX_LINE_CHARS).collect::<String>().replace('\t', "    ");
    if out.len() < line.len() && line.chars().nth(MAX_LINE_CHARS).is_some() {
        out.push('…');
    }
    out
}

fn load_image(path: &Path, max_side: usize) -> Content {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let decoded = image::ImageReader::open(path)
        .map_err(|e| e.to_string())
        .and_then(|r| r.with_guessed_format().map_err(|e| e.to_string()).and_then(|r| r.decode().map_err(|e| e.to_string())));
    let img = match decoded {
        Ok(img) => img,
        Err(e) => return Content::Error(format!("Cannot show {name} ({e}).")),
    };
    let size = [img.width() as usize, img.height() as usize];
    let max = max_side.clamp(256, MAX_SIDE) as u32;
    let (img, note) = if img.width() > max || img.height() > max {
        (img.thumbnail(max, max), Some(format!("Scaled down from {} × {} pixels.", size[0], size[1])))
    } else {
        (img, None)
    };
    let rgba = img.to_rgba8();
    let px = [rgba.width() as usize, rgba.height() as usize];
    Content::Image { image: egui::ColorImage::from_rgba_unmultiplied(px, rgba.as_raw()), size, note }
}

/// Every file below `artifacts/` with its modification time, at most
/// [`MAX_ARTIFACTS`]. Hidden names are left out.
fn artifacts(dir: &Path) -> HashMap<PathBuf, SystemTime> {
    let mut out = Vec::new();
    walk(dir, Path::new(ARTIFACTS), 0, &mut out);
    out.into_iter().collect()
}

fn walk(root: &Path, rel: &Path, depth: usize, out: &mut Vec<(PathBuf, SystemTime)>) {
    if depth > MAX_DEPTH || out.len() >= MAX_ARTIFACTS {
        return;
    }
    let Ok(read) = std::fs::read_dir(root.join(rel)) else { return };
    for e in read.flatten() {
        if out.len() >= MAX_ARTIFACTS {
            return;
        }
        let name = e.file_name().to_string_lossy().into_owned();
        let Ok(kind) = e.file_type() else { continue };
        if name.starts_with('.') {
            continue;
        }
        let path = rel.join(&name);
        if kind.is_dir() {
            walk(root, &path, depth + 1, out);
        } else if let Ok(meta) = std::fs::metadata(e.path())
            && meta.is_file()
            && let Ok(modified) = meta.modified()
        {
            out.push((path, modified));
        }
    }
}

impl ViewerTile {
    fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            shown: None,
            doc: None,
            loading: None,
            checked_at: 0.0,
            scanned_at: f64::NEG_INFINITY,
            known: None,
            fresh: None,
            seen_action: None,
            view: ImageView::default(),
            max_side: 2048,
        }
    }

    /// Shows `rel` from now on.
    fn choose(&mut self, rel: PathBuf) {
        if self.fresh.as_ref() == Some(&rel) {
            self.fresh = None;
        }
        if self.doc.as_ref().is_some_and(|d| d.rel != rel) {
            self.doc = None;
        }
        self.view = ImageView::default();
        self.shown = Some(rel);
        self.start_load();
    }

    fn start_load(&mut self) {
        let Some(rel) = self.shown.clone() else { return };
        let path = self.dir.join(&rel);
        let max_side = self.max_side;
        let (tx, rx) = mpsc::channel();
        let spawned = std::thread::Builder::new().name("viewer-load".into()).spawn(move || {
            let _ = tx.send(load(&path, max_side));
        });
        match spawned {
            Ok(_) => self.loading = Some((rel, rx)),
            Err(e) => tracing::warn!("cannot load a file for the viewer: {e}"),
        }
    }

    /// A result the agent wrote: shown at once when nothing is shown, else
    /// offered in the head. The shown file itself just reloads.
    fn offer(&mut self, rel: PathBuf) {
        if self.shown.as_ref() == Some(&rel) {
            self.checked_at = f64::NEG_INFINITY;
        } else if self.shown.is_none() {
            self.choose(rel);
        } else {
            self.fresh = Some(rel);
        }
    }

    fn poll(&mut self, ui: &Ui, info: &Info) {
        if let Some((rel, rx)) = &self.loading {
            match rx.try_recv() {
                Ok(loaded) => {
                    let rel = rel.clone();
                    self.loading = None;
                    if self.shown.as_ref() == Some(&rel) {
                        self.doc = Some(self.make_doc(ui, info, rel, loaded));
                    }
                }
                Err(mpsc::TryRecvError::Disconnected) => self.loading = None,
                Err(mpsc::TryRecvError::Empty) => ui.ctx().request_repaint_after(std::time::Duration::from_millis(50)),
            }
        }
    }

    fn make_doc(&mut self, ui: &Ui, info: &Info, rel: PathBuf, loaded: Loaded) -> Doc {
        let body = match loaded.content {
            Content::Image { image, size, note } => {
                let same = self.doc.as_ref().is_some_and(|d| d.rel == rel && matches!(&d.body, Body::Image { size: s, .. } if *s == size));
                if !same {
                    self.view = ImageView::default();
                }
                let texture = ui.ctx().load_texture(format!("viewer-{}", info.id), image, egui::TextureOptions::LINEAR);
                Body::Image { texture, size, note }
            }
            Content::Markdown(blocks) => Body::Markdown(blocks),
            Content::Csv { rows, note } => Body::Csv { rows, note, widths: None },
            Content::Text { lines, note } => {
                let widest = lines.iter().map(|l| l.chars().count()).max().unwrap_or(0);
                Body::Text { lines, note, widest }
            }
            Content::Link => Body::Link,
            Content::Binary => Body::Binary,
            Content::Error(e) => Body::Error(e),
        };
        Doc { rel, kind: loaded.kind, stamp: loaded.stamp, body }
    }

    /// New results: files that appeared or changed in `artifacts/`, and
    /// results the agent patched.
    fn watch(&mut self, info: &Info, actions: &[ActionEntry]) {
        if info.now - self.scanned_at >= SCAN_EVERY {
            self.scanned_at = info.now;
            let found = artifacts(&self.dir);
            let candidate = match &self.known {
                // The first scan only shows the newest result when nothing is chosen.
                None => found.iter().max_by_key(|(p, m)| (**m, (*p).clone())).map(|(p, _)| p.clone()).filter(|_| self.shown.is_none()),
                Some(known) => found
                    .iter()
                    .filter(|(p, m)| known.get(*p).is_none_or(|k| *m > k))
                    .max_by_key(|(p, m)| (**m, (*p).clone()))
                    .map(|(p, _)| p.clone()),
            };
            self.known = Some(found);
            if let Some(rel) = candidate {
                self.offer(rel);
            }
        }
        let first = self.seen_action.is_none();
        let mut candidate = None;
        for e in actions.iter().filter(|e| self.seen_action.is_none_or(|s| e.at > s)) {
            for rel in files::named_files(&self.dir, e.target()) {
                if rel.starts_with(ARTIFACTS) || kind_by_name(&rel).is_some() {
                    candidate = Some(rel);
                }
            }
        }
        self.seen_action = Some(actions.last().map_or(self.seen_action.unwrap_or(f64::NEG_INFINITY), |a| a.at));
        // Patches from before the Viewer opened are not news.
        if let Some(rel) = candidate.filter(|_| !first) {
            if let (Some(known), Some((modified, _))) = (&mut self.known, disk::stamp(&self.dir.join(&rel))) {
                known.insert(rel.clone(), modified);
            }
            self.offer(rel);
        }
    }

    /// Reloads the shown file when it changed on disk.
    fn follow(&mut self, now: f64) {
        if now - self.checked_at < CHECK_EVERY || self.loading.is_some() {
            return;
        }
        self.checked_at = now;
        let (Some(rel), Some(doc)) = (&self.shown, &self.doc) else { return };
        if disk::stamp(&self.dir.join(rel)) != doc.stamp {
            self.start_load();
        }
    }

    fn show(&mut self, ui: &mut Ui, info: &Info, actions: &[ActionEntry], head: Rect, body: Rect) -> Out {
        self.max_side = ui.ctx().input(|i| i.max_texture_side);
        self.poll(ui, info);
        self.watch(info, actions);
        self.follow(info.now);
        let mut out = Out::default();
        self.draw_head(ui, info, head, &mut out);

        // The tile's bottom corners are round, so the body is too.
        let corners = CornerRadius { nw: 0, ne: 0, sw: 13, se: 13 };
        let id = info.id.as_str();
        match (&self.shown, &mut self.doc) {
            (None, _) => {
                let text =
                    format!("Results that {} writes to artifacts/ show here. Open other files from Files with a double click.", info.name);
                super::super::agent::placeholder(ui, body, "Nothing to show yet", &text);
            }
            (Some(rel), None) => {
                let name = rel.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                super::super::agent::placeholder(ui, body, &format!("Opening {name}"), "One moment.");
            }
            (Some(_), Some(doc)) => {
                let rel = doc.rel.clone();
                let size = doc.stamp.map_or(0, |s| s.1);
                let kind = doc.kind;
                match &mut doc.body {
                    Body::Image { texture, size, note } => {
                        ui.painter_at(body).rect_filled(body, corners, c(theme::TERMINAL_BG));
                        draw_image(ui, body, texture, *size, note.as_deref(), &mut self.view, id);
                    }
                    Body::Markdown(blocks) => {
                        draw_markdown(ui, body, blocks, &rel, id, &mut out);
                    }
                    Body::Csv { rows, note, widths } => draw_csv(ui, body, rows, note.as_deref(), widths, &rel, id),
                    Body::Text { lines, note, widest } => draw_text(ui, body, lines, note.as_deref(), *widest, &rel, id),
                    Body::Link | Body::Binary => card(ui, body, &rel, kind, size, &self.dir, &mut out),
                    Body::Error(e) => {
                        let name = rel.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                        super::super::agent::placeholder(ui, body, &name, e);
                    }
                }
            }
        }
        if let Some(url) = out.link.take() {
            self.follow_link(&url, &mut out);
        }
        if ui.rect_contains_pointer(body) && ui.input(|i| i.pointer.any_pressed()) {
            out.clicked = true;
        }
        out
    }

    /// A clicked link: web pages go to the browser app, files inside the
    /// agent's directory open like a double click in Files.
    fn follow_link(&mut self, url: &str, out: &mut Out) {
        let lower = url.to_ascii_lowercase();
        if lower.starts_with("http://") || lower.starts_with("https://") || lower.starts_with("file://") {
            out.action = Some(Action::Browser(url.to_string()));
            return;
        }
        if lower.starts_with("mailto:") {
            if let Err(e) = files::open_external(Path::new(url)) {
                out.toast = Some((e, true));
            }
            return;
        }
        let path = percent_decode(url.split(['#', '?']).next().unwrap_or_default());
        if path.is_empty() {
            return;
        }
        let base = self.shown.as_ref().and_then(|s| s.parent()).map(Path::to_path_buf).unwrap_or_default();
        match resolve_link(&base, &path) {
            Some(rel) if self.dir.join(&rel).is_file() => out.action = Some(Action::Open(rel)),
            Some(rel) if self.dir.join(&rel).is_dir() => {
                out.toast = Some((format!("{} is a folder. Open it in Files.", rel.display()), false));
            }
            _ => out.toast = Some((format!("The link {url} leads to no file in the agent's directory."), true)),
        }
    }

    fn draw_head(&mut self, ui: &mut Ui, info: &Info, head: Rect, out: &mut Out) {
        let y = head.center().y;
        let x0 = head.min.x + 76.0;
        let kind = self.doc.as_ref().map(|d| d.kind).or_else(|| self.shown.as_ref().map(|r| kind_of(&self.dir.join(r))));
        let notice = self.fresh.as_ref().map(|_| format!("NEW FROM {}", info.name.to_uppercase()));
        let painter = ui.painter();
        let notice_w = notice
            .as_ref()
            .map_or(0.0, |n| painter.layout_no_wrap(n.clone(), theme::mono(10.5), Color32::WHITE).size().x + 14.0 + 8.0 + 58.0 + 10.0);
        let room = head.max.x - 10.0 - x0 - notice_w;
        let has_file = self.shown.is_some();
        let show_reload = has_file && room - 76.0 >= 90.0;
        let show_editor = show_reload && kind.is_some_and(Kind::is_text) && room - 76.0 - 122.0 >= 120.0;
        let show_you = room - if show_reload { 76.0 } else { 0.0 } - if show_editor { 122.0 } else { 0.0 } - 46.0 >= 200.0;

        let mut right = head.max.x - 10.0;
        if show_reload {
            let r = Rect::from_min_size(pos2(right - 70.0, y - 13.0), vec2(70.0, 26.0));
            if button(ui, r, "Reload", ButtonStyle::Quiet).on_hover_text("Read the file again").clicked() {
                self.start_load();
            }
            right = r.min.x - 6.0;
        }
        if show_editor {
            let r = Rect::from_min_size(pos2(right - 116.0, y - 13.0), vec2(116.0, 26.0));
            if button(ui, r, "Open in editor", ButtonStyle::Quiet).clicked()
                && let Some(rel) = &self.shown
            {
                out.action = Some(Action::Editor(rel.clone()));
            }
            right = r.min.x - 6.0;
        }
        if let (Some(notice), Some(fresh)) = (notice, self.fresh.clone()) {
            right -= 4.0;
            let r = Rect::from_min_size(pos2(right - 58.0, y - 13.0), vec2(58.0, 26.0));
            let shown = button(ui, r, "Show", ButtonStyle::Quiet).on_hover_text(format!("Show {}", fresh.display())).clicked();
            right = r.min.x - 8.0;
            let p = pill(ui.painter(), pos2(right, y), &notice, theme::WORKING);
            let hint = ui.interact(p, Id::new(("viewer-fresh", &info.id)), Sense::click()).on_hover_text(fresh.display().to_string());
            if hint.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
            if shown || hint.clicked() {
                self.choose(fresh);
            }
            right = p.min.x - 10.0;
        } else if show_you {
            let p = pill(ui.painter(), pos2(right - 2.0, y), "YOU", [0xB9, 0xBF, 0xCC]);
            right = p.min.x - 10.0;
        }

        let painter = ui.painter();
        let Some(rel) = &self.shown else {
            let text = fit(painter, "The newest result shows here", theme::ui(13.0), right - x0);
            painter.text(pos2(x0, y), Align2::LEFT_CENTER, text, theme::ui(13.0), c(theme::TEXT_2));
            return;
        };
        let name = rel.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let when = self.doc.as_ref().and_then(|d| d.stamp).map(|(m, _)| files::when(m, SystemTime::now())).unwrap_or_default();
        let when_w =
            if when.is_empty() { 0.0 } else { painter.layout_no_wrap(when.clone(), theme::mono(10.5), Color32::WHITE).size().x + 12.0 };
        let name_room = right - x0 - when_w;
        let name_text = fit(painter, &name, theme::ui_semibold(13.5), name_room.max(0.0));
        let r = painter.text(pos2(x0, y), Align2::LEFT_CENTER, &name_text, theme::ui_semibold(13.5), c(theme::TEXT));
        ui.interact(r, Id::new(("viewer-name", &info.id)), Sense::hover()).on_hover_text(rel.display().to_string());
        if !when.is_empty() && r.max.x + 12.0 + when_w - 12.0 <= right {
            ui.painter().text(pos2(r.max.x + 10.0, y), Align2::LEFT_CENTER, when.to_uppercase(), theme::mono(10.5), c(theme::TEXT_2));
        }
    }
}

/// The card for HTML, PDF and other binary files: what it is and where it opens.
fn card(ui: &mut Ui, body: Rect, rel: &Path, kind: Kind, size: u64, dir: &Path, out: &mut Out) {
    let name = rel.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let center = body.center();
    let painter = ui.painter_at(body);
    let label_w = painter.layout_no_wrap(kind.label().to_string(), theme::mono(10.5), Color32::WHITE).size().x + 14.0;
    pill(&painter, pos2(center.x + label_w / 2.0, center.y - 70.0), kind.label(), theme::NEEDS_YOU);
    let title = fit(&painter, &name, theme::ui_semibold(17.0), body.width() - 48.0);
    painter.text(pos2(center.x, center.y - 38.0), Align2::CENTER_CENTER, title, theme::ui_semibold(17.0), c(theme::TEXT));
    let line = match kind {
        Kind::Binary => format!("{} · meerora has no preview for this kind of file.", disk::human_size(size)),
        Kind::Pdf => format!("{} · PDF opens in the browser app or your PDF viewer.", disk::human_size(size)),
        _ => format!("{} · This page opens in the browser app or your web browser.", disk::human_size(size)),
    };
    let galley = painter.layout(line, theme::ui(13.5), c(theme::TEXT_2), (body.width() - 48.0).clamp(80.0, 460.0));
    painter.galley(pos2(center.x - galley.size().x / 2.0, center.y - 22.0), galley, c(theme::TEXT_2));
    let path = dir.join(rel);
    let system = Rect::from_center_size(pos2(center.x, center.y + 40.0), vec2(240.0, 34.0));
    if kind == Kind::Binary {
        if button(ui, system, "Open with the system viewer", ButtonStyle::Quiet).clicked()
            && let Err(e) = files::open_external(&path)
        {
            out.toast = Some((e, true));
        }
        return;
    }
    let browser = Rect::from_center_size(pos2(center.x, center.y + 32.0), vec2(240.0, 34.0));
    let system = browser.translate(vec2(0.0, 44.0));
    if button(ui, browser, "Open in the browser app", ButtonStyle::Primary).clicked() {
        out.action = Some(Action::Browser(file_url(&path)));
    }
    if button(ui, system, "Open with the system viewer", ButtonStyle::Quiet).clicked()
        && let Err(e) = files::open_external(&path)
    {
        out.toast = Some((e, true));
    }
}

/// A `file://` URL for an absolute path, with unsafe bytes percent-encoded.
pub(crate) fn file_url(path: &Path) -> String {
    let mut url = String::from("file://");
    for &b in path.as_os_str().as_encoded_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => url.push(b as char),
            _ => url.push_str(&format!("%{b:02X}")),
        }
    }
    url
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2]))
        {
            out.push((h * 16 + l) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `link` relative to the folder `base`, both inside the agent directory.
/// None when it climbs out of it or is absolute.
fn resolve_link(base: &Path, link: &str) -> Option<PathBuf> {
    let link = Path::new(link);
    if link.is_absolute() {
        return None;
    }
    let mut out = PathBuf::new();
    for part in base.components().chain(link.components()) {
        match part {
            Component::Normal(p) => out.push(p),
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    return None;
                }
            }
            _ => return None,
        }
    }
    (!out.as_os_str().is_empty()).then_some(out)
}

// ---------------------------------------------------------------- images

fn draw_image(ui: &mut Ui, r: Rect, texture: &egui::TextureHandle, size: [usize; 2], note: Option<&str>, view: &mut ImageView, id: &str) {
    let ppp = ui.ctx().pixels_per_point();
    let natural = vec2(size[0].max(1) as f32, size[1].max(1) as f32) / ppp;
    let room = r.shrink(16.0);
    let fit_scale = (room.width() / natural.x).min(room.height() / natural.y).clamp(0.01, 1.0);
    let scale = view.zoom.unwrap_or(fit_scale);
    let resp = ui.interact(r, Id::new(("viewer-image", id)), Sense::click_and_drag());

    // Scroll zooms around the pointer, dragging pans, a double click fits or shows 100 %.
    let mut zoom_to = None;
    if resp.hovered() {
        let (scroll, pinch) = ui.input(|i| (i.smooth_scroll_delta.y, i.zoom_delta()));
        let factor = (scroll * 0.002).exp() * pinch;
        if (factor - 1.0).abs() > 1e-4 {
            zoom_to = Some((scale * factor, ui.ctx().pointer_hover_pos().unwrap_or(r.center())));
        }
    }
    if resp.dragged() {
        view.zoom = Some(scale);
        view.pan += resp.drag_delta();
        ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
    } else if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
    }
    if resp.double_clicked() {
        *view = if view.zoom.is_some() { ImageView::default() } else { ImageView { zoom: Some(1.0), pan: Vec2::ZERO } };
    }

    // The zoom bar at the bottom right.
    let bar = Rect::from_min_size(pos2(r.max.x - 12.0 - 196.0, r.max.y - 12.0 - 32.0), vec2(196.0, 32.0));
    let painter = ui.painter_at(r);
    let scale_now = view.zoom.unwrap_or(fit_scale);
    let center = r.center() + view.pan;
    let shown = Rect::from_center_size(center, natural * scale_now);
    painter.image(texture.id(), shown, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
    if let Some(note) = note {
        let galley = painter.layout_no_wrap(note.to_string(), theme::mono(10.5), c(theme::TEXT_2));
        let chip = Rect::from_min_size(pos2(r.min.x + 12.0, r.max.y - 12.0 - 24.0), galley.size() + vec2(16.0, 8.0));
        painter.rect_filled(chip, CornerRadius::same(8), ca(theme::BG, 220));
        painter.galley(chip.min + vec2(8.0, 4.0), galley, c(theme::TEXT_2));
    }
    painter.rect(bar, CornerRadius::same(10), ca(theme::PANEL, 235), Stroke::new(1.0, c(theme::BORDER)), StrokeKind::Inside);
    let b = |i: f32, w: f32| Rect::from_min_size(pos2(bar.min.x + 3.0 + i, bar.min.y + 3.0), vec2(w, 26.0));
    // Minus and plus are drawn, the glyphs are too small in the UI font.
    let sign = Stroke::new(1.6, c(theme::TEXT_2));
    if button(ui, b(0.0, 28.0), "", ButtonStyle::Ghost).on_hover_text("Zoom out").clicked() {
        zoom_to = Some((scale_now / 1.25, r.center()));
    }
    let m = b(0.0, 28.0).center();
    ui.painter().line_segment([m - vec2(5.0, 0.0), m + vec2(5.0, 0.0)], sign);
    let pct = format!("{}%", (scale_now * 100.0).round() as i32);
    ui.painter().text(b(30.0, 52.0).center(), Align2::CENTER_CENTER, pct, theme::mono(11.0), c(theme::TEXT));
    if button(ui, b(84.0, 28.0), "", ButtonStyle::Ghost).on_hover_text("Zoom in").clicked() {
        zoom_to = Some((scale_now * 1.25, r.center()));
    }
    let p = b(84.0, 28.0).center();
    ui.painter().line_segment([p - vec2(5.0, 0.0), p + vec2(5.0, 0.0)], sign);
    ui.painter().line_segment([p - vec2(0.0, 5.0), p + vec2(0.0, 5.0)], sign);
    let fit_style = if view.zoom.is_none() { ButtonStyle::Selected } else { ButtonStyle::Ghost };
    if button(ui, b(114.0, 38.0), "Fit", fit_style).on_hover_text("Fit the image to the tile").clicked() {
        *view = ImageView::default();
    }
    if button(ui, b(154.0, 36.0), "1:1", ButtonStyle::Ghost).on_hover_text("Show the image at 100 %").clicked() {
        *view = ImageView { zoom: Some(1.0), pan: Vec2::ZERO };
    }
    if let Some((to, around)) = zoom_to {
        zoom_view(view, scale_now, to, around - r.center());
    }
    // Keep part of the image in the tile.
    let s = view.zoom.unwrap_or(fit_scale);
    let limit = (natural * s + r.size()) / 2.0 - vec2(40.0, 40.0);
    view.pan = vec2(view.pan.x.clamp(-limit.x.max(0.0), limit.x.max(0.0)), view.pan.y.clamp(-limit.y.max(0.0), limit.y.max(0.0)));
}

/// Zooms from `from` to `to`, keeping the point `around` (from the tile's
/// centre) where it is.
fn zoom_view(view: &mut ImageView, from: f32, to: f32, around: Vec2) {
    let to = to.clamp(0.02, 16.0);
    let image_point = (around - view.pan) / from;
    view.pan = around - image_point * to;
    view.zoom = Some(to);
}

// -------------------------------------------------------------- Markdown

/// Inline style of a run of text.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Style {
    pub bold: bool,
    pub italic: bool,
    pub strike: bool,
    pub code: bool,
}

/// A run of text with one style, and the link it belongs to.
#[derive(Clone, Debug, PartialEq)]
pub struct Span {
    pub text: String,
    pub style: Style,
    pub link: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CellAlign {
    Left,
    Center,
    Right,
}

/// A block of a Markdown document as the Viewer draws it.
#[derive(Clone, Debug, PartialEq)]
pub enum Block {
    Heading(u8, Vec<Span>),
    Paragraph(Vec<Span>),
    /// A list item at `depth` from 0, with its marker such as "•" or "3.".
    /// Further paragraphs of the same item have an empty marker.
    Item {
        depth: usize,
        marker: String,
        spans: Vec<Span>,
    },
    Code {
        lang: String,
        text: String,
    },
    Quote(Vec<Span>),
    Table {
        align: Vec<CellAlign>,
        head: Vec<Vec<Span>>,
        rows: Vec<Vec<Vec<Span>>>,
    },
    Rule,
}

/// Converts Markdown into blocks: CommonMark with tables, strikethrough and task lists.
pub fn markdown_blocks(text: &str) -> Vec<Block> {
    use pulldown_cmark::{Options, Parser};
    let options = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let mut b = MdBuilder::default();
    for event in Parser::new_ext(text, options) {
        b.event(event);
        if b.blocks.len() > MAX_BLOCKS {
            break;
        }
    }
    b.flush();
    if b.blocks.len() > MAX_BLOCKS {
        b.blocks.truncate(MAX_BLOCKS);
        b.blocks.push(Block::Paragraph(vec![Span {
            text: format!("The Viewer shows the first {MAX_BLOCKS} blocks of this document."),
            style: Style { italic: true, ..Style::default() },
            link: None,
        }]));
    }
    b.blocks
}

#[derive(Default)]
struct MdBuilder {
    blocks: Vec<Block>,
    spans: Vec<Span>,
    bold: usize,
    italic: usize,
    strike: usize,
    links: Vec<String>,
    /// Open lists with the number of the next item, `None` for bullets.
    lists: Vec<Option<u64>>,
    /// Open list items.
    items: usize,
    /// Marker of the innermost item while its first line is not out yet.
    marker: Option<String>,
    quote: usize,
    heading: Option<u8>,
    code: Option<(String, String)>,
    table: Option<MdTable>,
}

#[derive(Default)]
struct MdTable {
    align: Vec<CellAlign>,
    head: Vec<Vec<Span>>,
    rows: Vec<Vec<Vec<Span>>>,
    row: Vec<Vec<Span>>,
    /// Rows or columns beyond the limits were left out.
    cut: bool,
}

impl MdBuilder {
    fn push(&mut self, text: &str, code: bool) {
        if text.is_empty() {
            return;
        }
        let style = Style { bold: self.bold > 0, italic: self.italic > 0, strike: self.strike > 0, code };
        let link = self.links.last().cloned();
        if let Some(last) = self.spans.last_mut()
            && last.style == style
            && last.link == link
        {
            last.text.push_str(text);
            return;
        }
        self.spans.push(Span { text: text.to_string(), style, link });
    }

    /// Ends the text so far as a list item, a quote or a paragraph.
    fn flush(&mut self) {
        let spans = std::mem::take(&mut self.spans);
        if self.items > 0 {
            let marker = self.marker.take();
            if spans.is_empty() && marker.is_none() {
                return;
            }
            self.blocks.push(Block::Item { depth: self.lists.len().saturating_sub(1), marker: marker.unwrap_or_default(), spans });
        } else if spans.is_empty() {
        } else if self.quote > 0 {
            self.blocks.push(Block::Quote(spans));
        } else {
            self.blocks.push(Block::Paragraph(spans));
        }
    }

    fn event(&mut self, event: pulldown_cmark::Event) {
        use pulldown_cmark::{CodeBlockKind, Event, Tag, TagEnd};
        match event {
            Event::Start(tag) => match tag {
                Tag::Heading { level, .. } => {
                    self.flush();
                    self.heading = Some(level as u8);
                }
                Tag::BlockQuote(_) => {
                    self.flush();
                    self.quote += 1;
                }
                Tag::CodeBlock(kind) => {
                    self.flush();
                    let lang = match kind {
                        CodeBlockKind::Fenced(info) => info.split_whitespace().next().unwrap_or_default().to_string(),
                        CodeBlockKind::Indented => String::new(),
                    };
                    self.code = Some((lang, String::new()));
                }
                Tag::List(start) => {
                    if self.items > 0 {
                        self.flush();
                    }
                    self.lists.push(start);
                }
                Tag::Item => {
                    self.flush();
                    let marker = match self.lists.last_mut() {
                        Some(Some(n)) => {
                            let m = format!("{n}.");
                            *n += 1;
                            m
                        }
                        _ => "•".to_string(),
                    };
                    self.items += 1;
                    self.marker = Some(marker);
                }
                Tag::Table(align) => {
                    self.flush();
                    let align = align
                        .iter()
                        .map(|a| match a {
                            pulldown_cmark::Alignment::Center => CellAlign::Center,
                            pulldown_cmark::Alignment::Right => CellAlign::Right,
                            _ => CellAlign::Left,
                        })
                        .collect();
                    self.table = Some(MdTable { align, ..Default::default() });
                }
                Tag::TableCell => self.spans.clear(),
                Tag::Emphasis => self.italic += 1,
                Tag::Strong => self.bold += 1,
                Tag::Strikethrough => self.strike += 1,
                Tag::Link { dest_url, .. } => self.links.push(dest_url.to_string()),
                Tag::Image { dest_url, .. } => {
                    self.links.push(dest_url.to_string());
                    self.push("Image: ", false);
                }
                _ => {}
            },
            Event::End(end) => match end {
                TagEnd::Paragraph => self.flush(),
                TagEnd::Heading(_) => {
                    let spans = std::mem::take(&mut self.spans);
                    self.blocks.push(Block::Heading(self.heading.take().unwrap_or(1), spans));
                }
                TagEnd::BlockQuote(_) => {
                    self.flush();
                    self.quote = self.quote.saturating_sub(1);
                }
                TagEnd::CodeBlock => {
                    if let Some((lang, mut text)) = self.code.take() {
                        if text.ends_with('\n') {
                            text.pop();
                        }
                        self.blocks.push(Block::Code { lang, text });
                    }
                }
                TagEnd::List(_) => {
                    self.lists.pop();
                }
                TagEnd::Item => {
                    self.flush();
                    self.items = self.items.saturating_sub(1);
                    self.marker = None;
                }
                TagEnd::TableCell => {
                    let spans = std::mem::take(&mut self.spans);
                    if let Some(t) = &mut self.table {
                        if t.row.len() < MAX_TABLE_COLUMNS {
                            t.row.push(spans);
                        } else {
                            t.cut = true;
                        }
                    }
                }
                TagEnd::TableHead => {
                    if let Some(t) = &mut self.table {
                        t.head = std::mem::take(&mut t.row);
                    }
                }
                TagEnd::TableRow => {
                    if let Some(t) = &mut self.table {
                        let row = std::mem::take(&mut t.row);
                        if t.rows.len() < MAX_TABLE_ROWS {
                            t.rows.push(row);
                        } else {
                            t.cut = true;
                        }
                    }
                }
                TagEnd::Table => {
                    if let Some(mut t) = self.table.take() {
                        t.align.truncate(MAX_TABLE_COLUMNS);
                        self.blocks.push(Block::Table { align: t.align, head: t.head, rows: t.rows });
                        if t.cut {
                            let text =
                                format!("The Viewer shows the first {MAX_TABLE_ROWS} rows and {MAX_TABLE_COLUMNS} columns of this table.");
                            self.blocks.push(Block::Paragraph(vec![Span {
                                text,
                                style: Style { italic: true, ..Style::default() },
                                link: None,
                            }]));
                        }
                    }
                }
                TagEnd::Emphasis => self.italic = self.italic.saturating_sub(1),
                TagEnd::Strong => self.bold = self.bold.saturating_sub(1),
                TagEnd::Strikethrough => self.strike = self.strike.saturating_sub(1),
                TagEnd::Link | TagEnd::Image => {
                    self.links.pop();
                }
                _ => {}
            },
            Event::Text(text) => match &mut self.code {
                Some((_, code)) => code.push_str(&text),
                None => self.push(&text, false),
            },
            Event::Code(text) | Event::InlineMath(text) | Event::DisplayMath(text) => self.push(&text, true),
            Event::SoftBreak => self.push(" ", false),
            Event::HardBreak => self.push("\n", false),
            Event::Rule => {
                self.flush();
                self.blocks.push(Block::Rule);
            }
            Event::TaskListMarker(done) => self.push(if done { "[x] " } else { "[ ] " }, true),
            Event::InlineHtml(html) => {
                let tag = html.trim().to_ascii_lowercase();
                if matches!(tag.as_str(), "<br>" | "<br/>" | "<br />") {
                    self.push("\n", false);
                }
            }
            Event::FootnoteReference(name) => self.push(&format!("[{name}]"), false),
            Event::Html(_) => {}
        }
    }
}

/// Fonts and colours of a run of rich text.
struct RichStyle {
    size: f32,
    bold: bool,
    color: Color32,
}

/// A laid out text and the character ranges of its links.
type Rich = (Arc<Galley>, Vec<(Range<usize>, String)>);

/// Lays out spans as one wrapped text, with the character ranges of links.
fn layout_spans(ui: &Ui, spans: &[Span], style: &RichStyle, width: f32) -> Rich {
    let mut job = LayoutJob::default();
    job.wrap.max_width = width.max(20.0);
    let mut links = Vec::new();
    let mut chars = 0;
    for span in spans {
        let bold = style.bold || span.style.bold;
        let font = if span.style.code {
            theme::mono(style.size - 1.5)
        } else if bold {
            theme::ui_bold(style.size)
        } else {
            theme::ui(style.size)
        };
        let color = if span.link.is_some() { c(theme::NEEDS_YOU) } else { style.color };
        let format = TextFormat {
            font_id: font,
            color,
            italics: span.style.italic,
            background: if span.style.code { c([0x1E, 0x23, 0x2E]) } else { Color32::TRANSPARENT },
            underline: if span.link.is_some() { Stroke::new(1.0, ca(theme::NEEDS_YOU, 120)) } else { Stroke::NONE },
            strikethrough: if span.style.strike { Stroke::new(1.0, style.color) } else { Stroke::NONE },
            line_height: Some(style.size * 1.45),
            ..Default::default()
        };
        job.append(&span.text, 0.0, format);
        let n = span.text.chars().count();
        if let Some(link) = &span.link {
            links.push((chars..chars + n, link.clone()));
        }
        chars += n;
    }
    let galley = ui.fonts_mut(|f| f.layout_job(job));
    (galley, links)
}

/// Paints a laid out text at `pos` and reports a clicked link.
fn paint_rich(ui: &mut Ui, galley: Arc<Galley>, links: &[(Range<usize>, String)], pos: egui::Pos2, id: Id, out: &mut Out) {
    let rect = Rect::from_min_size(pos, galley.size());
    if !links.is_empty() {
        let resp = ui.interact(rect, id, Sense::click());
        if let Some(p) = resp.hover_pos() {
            let local = p - rect.min;
            let in_row = galley.rows.iter().any(|r| r.rect().expand2(vec2(0.0, 1.0)).contains(pos2(local.x, local.y)));
            let at = galley.cursor_from_pos(local).index.0;
            if let Some((_, url)) = links.iter().find(|(range, _)| in_row && range.contains(&at)) {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                let url = url.clone();
                if resp.clicked() {
                    out.link = Some(url.clone());
                }
                resp.on_hover_text_at_pointer(url);
            }
        }
    }
    ui.painter().galley(rect.min, galley, c(BODY_TEXT));
}

/// One block of text that wraps at `width`, allocated in `ui`.
fn rich_block(ui: &mut Ui, spans: &[Span], style: &RichStyle, width: f32, indent: f32, id: Id, out: &mut Out) -> Rect {
    let (galley, links) = layout_spans(ui, spans, style, width - indent);
    let (rect, _) = ui.allocate_exact_size(vec2(width, galley.size().y), Sense::hover());
    let pos = pos2(rect.min.x + indent, rect.min.y);
    paint_rich(ui, galley, &links, pos, id, out);
    rect
}

fn draw_markdown(ui: &mut Ui, r: Rect, blocks: &[Block], rel: &Path, id: &str, out: &mut Out) {
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(r).layout(egui::Layout::top_down(egui::Align::Min)));
    child.set_clip_rect(r.intersect(ui.clip_rect()));
    egui::ScrollArea::vertical().id_salt(("viewer-md", id, rel)).auto_shrink([false, false]).show(&mut child, |ui| {
        let width = (r.width() - 24.0 - 28.0).clamp(80.0, 800.0);
        egui::Frame::NONE.inner_margin(egui::Margin { left: 24, right: 12, top: 18, bottom: 28 }).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            ui.set_width(width);
            if blocks.is_empty() {
                ui.label(egui::RichText::new("The file is empty").font(theme::ui(14.0)).color(c(theme::TEXT_2)));
            }
            let mut previous: Option<&Block> = None;
            for (i, block) in blocks.iter().enumerate() {
                let bid = Id::new(("viewer-md-block", id, i));
                let after_list = matches!(previous, Some(Block::Item { .. })) && !matches!(block, Block::Item { .. });
                if after_list {
                    ui.add_space(8.0);
                }
                draw_block(ui, block, width, previous.is_none(), bid, out);
                previous = Some(block);
            }
        });
    });
}

fn draw_block(ui: &mut Ui, block: &Block, width: f32, first: bool, id: Id, out: &mut Out) {
    let body = RichStyle { size: 14.5, bold: false, color: c(BODY_TEXT) };
    match block {
        Block::Heading(level, spans) => {
            let (size, above) = match level {
                1 => (23.0, 6.0),
                2 => (19.0, 18.0),
                3 => (16.5, 14.0),
                _ => (14.5, 12.0),
            };
            if !first {
                ui.add_space(above);
            }
            let style = RichStyle { size, bold: true, color: c(theme::TEXT) };
            let rect = rich_block(ui, spans, &style, width, 0.0, id, out);
            if *level <= 2 {
                ui.add_space(6.0);
                super::super::hline(ui.painter(), rect.max.y + 5.0, rect.min.x, rect.min.x + width);
            }
            ui.add_space(8.0);
        }
        Block::Paragraph(spans) => {
            rich_block(ui, spans, &body, width, 0.0, id, out);
            ui.add_space(10.0);
        }
        Block::Item { depth, marker, spans } => {
            let indent = *depth as f32 * 20.0;
            let marker_w = if marker.ends_with('.') { 26.0 } else { 18.0 };
            let rect = rich_block(ui, spans, &body, width, indent + marker_w, id, out);
            // Centred on the first line, whose height the layout fixes.
            let y = rect.min.y + body.size * 1.45 / 2.0;
            let painter = ui.painter();
            if marker.ends_with('.') {
                painter.text(pos2(rect.min.x + indent, y), Align2::LEFT_CENTER, marker, theme::mono(12.5), c(theme::TEXT_2));
            } else if !marker.is_empty() && *depth == 0 {
                painter.circle_filled(pos2(rect.min.x + indent + 5.0, y), 2.5, c(theme::TEXT_2));
            } else if !marker.is_empty() {
                painter.circle_stroke(pos2(rect.min.x + indent + 5.0, y), 2.5, Stroke::new(1.2, c(theme::TEXT_2)));
            }
            ui.add_space(4.0);
        }
        Block::Code { lang, text } => {
            let mut job = LayoutJob::simple(text.clone(), theme::mono(12.5), c([0xC9, 0xCE, 0xD9]), width - 28.0);
            job.wrap.max_width = width - 28.0;
            let galley = ui.fonts_mut(|f| f.layout_job(job));
            let (rect, _) = ui.allocate_exact_size(vec2(width, galley.size().y + 22.0), Sense::hover());
            let painter = ui.painter();
            painter.rect(rect, CornerRadius::same(8), c(theme::TERMINAL_BG), Stroke::new(1.0, c(theme::BORDER)), StrokeKind::Inside);
            painter.galley(rect.min + vec2(14.0, 11.0), galley, c([0xC9, 0xCE, 0xD9]));
            if !lang.is_empty() {
                painter.text(rect.right_top() + vec2(-10.0, 7.0), Align2::RIGHT_TOP, lang.to_uppercase(), theme::mono(9.5), c(LINE_NO));
            }
            ui.add_space(12.0);
        }
        Block::Quote(spans) => {
            let style = RichStyle { size: 14.5, bold: false, color: c(theme::TEXT_2) };
            let rect = rich_block(ui, spans, &style, width, 16.0, id, out);
            ui.painter().rect_filled(Rect::from_min_size(rect.min, vec2(3.0, rect.height())), CornerRadius::same(1), c([0x33, 0x3A, 0x48]));
            ui.add_space(10.0);
        }
        Block::Table { align, head, rows } => {
            draw_table(ui, align, head, rows, width, id, out);
            ui.add_space(12.0);
        }
        Block::Rule => {
            let (rect, _) = ui.allocate_exact_size(vec2(width, 20.0), Sense::hover());
            super::super::hline(ui.painter(), rect.center().y, rect.min.x, rect.max.x);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_table(ui: &mut Ui, align: &[CellAlign], head: &[Vec<Span>], rows: &[Vec<Vec<Span>>], width: f32, id: Id, out: &mut Out) {
    let columns = head.len().max(rows.iter().map(Vec::len).max().unwrap_or(0));
    if columns == 0 {
        return;
    }
    const PAD_X: f32 = 12.0;
    const PAD_Y: f32 = 7.0;
    let max_cell = (width / columns as f32).clamp(160.0, 360.0) - 2.0 * PAD_X;
    let cell_style = RichStyle { size: 13.5, bold: false, color: c(BODY_TEXT) };
    let head_style = RichStyle { size: 13.5, bold: true, color: c(theme::TEXT) };
    // Lay out every cell first, for the column widths and row heights.
    let mut laid: Vec<Vec<Rich>> = Vec::new();
    for (r, row) in std::iter::once(head).chain(rows.iter().map(Vec::as_slice)).enumerate() {
        let style = if r == 0 { &head_style } else { &cell_style };
        laid.push((0..columns).map(|k| layout_spans(ui, row.get(k).map_or(&[][..], Vec::as_slice), style, max_cell)).collect());
    }
    let mut widths = vec![24.0f32; columns];
    for row in &laid {
        for (k, (g, _)) in row.iter().enumerate() {
            widths[k] = widths[k].max(g.size().x.ceil() + 2.0 * PAD_X);
        }
    }
    let heights: Vec<f32> = laid.iter().map(|row| row.iter().map(|(g, _)| g.size().y).fold(0.0, f32::max) + 2.0 * PAD_Y).collect();
    let total = vec2(widths.iter().sum(), heights.iter().sum());
    egui::ScrollArea::horizontal().id_salt(id.with("scroll")).auto_shrink([false, true]).show(ui, |ui| {
        let (rect, _) = ui.allocate_exact_size(vec2(total.x.max(1.0), total.y), Sense::hover());
        let painter = ui.painter().clone();
        painter.rect(rect, CornerRadius::same(8), c(theme::PANEL), Stroke::NONE, StrokeKind::Inside);
        let head_rect = Rect::from_min_size(rect.min, vec2(total.x, heights[0]));
        painter.rect_filled(head_rect, CornerRadius { nw: 8, ne: 8, sw: 0, se: 0 }, c([0x1A, 0x1F, 0x29]));
        let mut y = rect.min.y;
        for (r, row) in laid.into_iter().enumerate() {
            let mut x = rect.min.x;
            if r > 0 {
                painter.line_segment([pos2(rect.min.x, y), pos2(rect.max.x, y)], Stroke::new(1.0, c([0x22, 0x27, 0x32])));
            }
            for (k, (galley, links)) in row.into_iter().enumerate() {
                let cell_w = widths[k] - 2.0 * PAD_X;
                let dx = match align.get(k).copied().unwrap_or(CellAlign::Left) {
                    CellAlign::Left => 0.0,
                    CellAlign::Center => (cell_w - galley.size().x) / 2.0,
                    CellAlign::Right => cell_w - galley.size().x,
                };
                paint_rich(ui, galley, &links, pos2(x + PAD_X + dx.max(0.0), y + PAD_Y), id.with((r, k)), out);
                x += widths[k];
            }
            y += heights[r];
        }
        painter.rect_stroke(rect, CornerRadius::same(8), Stroke::new(1.0, c(theme::BORDER)), StrokeKind::Inside);
    });
}

// ------------------------------------------------------------------- CSV

/// The separator of a CSV file: comma, semicolon or tab, whichever its first
/// line has most of outside quotes.
pub fn sniff_separator(text: &str) -> char {
    let mut counts = [(',', 0usize), (';', 0), ('\t', 0)];
    let mut quoted = false;
    for ch in text.chars() {
        match ch {
            '"' => quoted = !quoted,
            '\n' if !quoted => break,
            _ if !quoted => {
                for (sep, n) in &mut counts {
                    if ch == *sep {
                        *n += 1;
                    }
                }
            }
            _ => {}
        }
    }
    counts.iter().max_by_key(|(sep, n)| (*n, *sep == ',')).map_or(',', |(sep, _)| *sep)
}

/// Splits CSV text into rows of fields, at most `max_rows`. Quoted fields
/// may hold separators, line breaks and doubled quotes. Empty lines are left
/// out. A row keeps its first [`MAX_COLUMNS`] fields, a field its first
/// [`MAX_FIELD_CHARS`] characters.
pub fn parse_csv(text: &str, sep: char, max_rows: usize) -> Vec<Vec<String>> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut field = String::new();
    // Characters seen in the field, also those beyond the limit.
    let mut field_chars = 0usize;
    let mut quoted = false;
    // The field began with a quote or a character, so a later quote is literal.
    let mut field_started = false;
    // The line holds anything, so it is a row even when all its fields are empty.
    let mut line_used = false;
    let mut chars = text.chars().peekable();
    let push = |field: &mut String, field_chars: &mut usize, ch: char| {
        if *field_chars < MAX_FIELD_CHARS {
            field.push(ch);
        }
        *field_chars += 1;
    };
    let end_field = |row: &mut Vec<String>, field: &mut String, field_chars: &mut usize| {
        let done = std::mem::take(field);
        *field_chars = 0;
        if row.len() < MAX_COLUMNS {
            row.push(done);
        }
    };
    while let Some(ch) = chars.next() {
        if rows.len() >= max_rows {
            return rows;
        }
        if quoted {
            if ch != '"' {
                push(&mut field, &mut field_chars, ch);
            } else if chars.peek() == Some(&'"') {
                push(&mut field, &mut field_chars, '"');
                chars.next();
            } else {
                quoted = false;
            }
            continue;
        }
        if ch == '"' && !field_started {
            quoted = true;
            field_started = true;
            line_used = true;
        } else if ch == sep {
            end_field(&mut row, &mut field, &mut field_chars);
            field_started = false;
            line_used = true;
        } else if ch == '\n' || ch == '\r' {
            if ch == '\r' && chars.peek() == Some(&'\n') {
                chars.next();
            }
            if line_used {
                end_field(&mut row, &mut field, &mut field_chars);
                rows.push(std::mem::take(&mut row));
            }
            field_started = false;
            line_used = false;
        } else {
            push(&mut field, &mut field_chars, ch);
            field_started = true;
            line_used = true;
        }
    }
    if line_used && rows.len() < max_rows {
        end_field(&mut row, &mut field, &mut field_chars);
        rows.push(row);
    }
    rows
}

fn draw_csv(ui: &mut Ui, r: Rect, rows: &[Vec<String>], note: Option<&str>, widths: &mut Option<Vec<f32>>, rel: &Path, id: &str) {
    if rows.is_empty() {
        super::super::agent::placeholder(ui, r, "No rows", "The file has no data.");
        return;
    }
    let columns = rows.iter().take(500).map(Vec::len).max().unwrap_or(0).max(1);
    let widths = widths.get_or_insert_with(|| {
        let painter = ui.painter();
        let mut w = vec![40.0f32; columns];
        for (i, row) in rows.iter().take(200).enumerate() {
            let font = if i == 0 { theme::ui_semibold(13.0) } else { theme::mono(12.0) };
            for (k, cell) in row.iter().enumerate().take(columns) {
                let text: String = cell.chars().take(80).collect();
                let size = painter.layout_no_wrap(text, font.clone(), Color32::WHITE).size().x;
                w[k] = w[k].max((size + 24.0).min(320.0));
            }
        }
        w
    });
    let total_w: f32 = widths.iter().sum::<f32>() + 16.0;
    let (r, note_r) = match note {
        Some(_) => {
            (Rect::from_min_max(r.min, pos2(r.max.x, r.max.y - 26.0)), Some(Rect::from_min_max(pos2(r.min.x, r.max.y - 26.0), r.max)))
        }
        None => (r, None),
    };
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(r).layout(egui::Layout::top_down(egui::Align::Min)));
    child.set_clip_rect(r.intersect(ui.clip_rect()));
    child.spacing_mut().item_spacing.y = 0.0;
    egui::ScrollArea::both().id_salt(("viewer-csv", id, rel)).auto_shrink([false, false]).show_rows(
        &mut child,
        CSV_ROW_H,
        rows.len(),
        |ui, range| {
            for i in range {
                let (row_r, _) = ui.allocate_exact_size(vec2(total_w, CSV_ROW_H), Sense::hover());
                let painter = ui.painter();
                if i == 0 {
                    painter.rect_filled(row_r, CornerRadius::ZERO, c([0x1A, 0x1F, 0x29]));
                } else if i % 2 == 0 {
                    painter.rect_filled(row_r, CornerRadius::ZERO, c([0x14, 0x18, 0x21]));
                }
                painter.line_segment(
                    [pos2(row_r.min.x, row_r.max.y - 0.5), pos2(row_r.max.x, row_r.max.y - 0.5)],
                    Stroke::new(1.0, c([0x1C, 0x20, 0x29])),
                );
                let mut x = row_r.min.x + 8.0;
                for (k, w) in widths.iter().enumerate() {
                    let cell = rows[i].get(k).map(String::as_str).unwrap_or("");
                    let (font, color) = if i == 0 { (theme::ui_semibold(13.0), c(theme::TEXT)) } else { (theme::mono(12.0), c(BODY_TEXT)) };
                    let head: String = cell.lines().next().unwrap_or_default().chars().take(CELL_CHARS).collect();
                    let shown = fit(painter, &head, font.clone(), w - 20.0);
                    let numeric = i > 0 && cell.trim().parse::<f64>().is_ok();
                    if numeric {
                        painter.text(pos2(x + w - 12.0, row_r.center().y), Align2::RIGHT_CENTER, shown, font, color);
                    } else {
                        painter.text(pos2(x + 10.0, row_r.center().y), Align2::LEFT_CENTER, shown, font, color);
                    }
                    x += w;
                    painter.line_segment([pos2(x, row_r.min.y), pos2(x, row_r.max.y)], Stroke::new(1.0, c([0x1C, 0x20, 0x29])));
                }
            }
        },
    );
    if let (Some(note), Some(nr)) = (note, note_r) {
        let painter = ui.painter_at(nr);
        super::super::hline(&painter, nr.min.y + 0.5, nr.min.x, nr.max.x);
        painter.text(pos2(nr.min.x + 12.0, nr.center().y), Align2::LEFT_CENTER, note, theme::mono(11.0), c(DIM));
    }
}

// ------------------------------------------------------------------ text

fn draw_text(ui: &mut Ui, r: Rect, lines: &[String], note: Option<&str>, widest: usize, rel: &Path, id: &str) {
    let (r, note_r) = match note {
        Some(_) => (Rect::from_min_max(pos2(r.min.x, r.min.y + 28.0), r.max), Some(Rect::from_min_size(r.min, vec2(r.width(), 28.0)))),
        None => (r, None),
    };
    let font = theme::mono(12.5);
    let char_w = ui.painter().layout_no_wrap("M".into(), font.clone(), Color32::WHITE).size().x;
    let gutter = 12.0 + char_w * (lines.len().max(1).ilog10() as f32 + 1.0) + 14.0;
    let total_w = gutter + char_w * widest as f32 + 32.0;
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(r).layout(egui::Layout::top_down(egui::Align::Min)));
    child.set_clip_rect(r.intersect(ui.clip_rect()));
    child.spacing_mut().item_spacing.y = 0.0;
    egui::ScrollArea::both().id_salt(("viewer-text", id, rel)).auto_shrink([false, false]).show_rows(
        &mut child,
        TEXT_LINE_H,
        lines.len(),
        |ui, range| {
            if range.start == 0 {
                ui.add_space(0.0);
            }
            for i in range {
                let (row, _) = ui.allocate_exact_size(vec2(total_w.max(r.width()), TEXT_LINE_H), Sense::hover());
                let painter = ui.painter();
                painter.text(
                    pos2(row.min.x + gutter - 14.0, row.center().y),
                    Align2::RIGHT_CENTER,
                    format!("{}", i + 1),
                    theme::mono(11.5),
                    c(LINE_NO),
                );
                painter.text(pos2(row.min.x + gutter, row.center().y), Align2::LEFT_CENTER, &lines[i], font.clone(), c(BODY_TEXT));
            }
        },
    );
    if let (Some(note), Some(nr)) = (note, note_r) {
        let painter = ui.painter_at(nr);
        painter.rect_filled(nr, CornerRadius::ZERO, ca(theme::WORKING, 16));
        super::super::hline(&painter, nr.max.y - 0.5, nr.min.x, nr.max.x);
        let text = fit(&painter, note, theme::ui(12.5), nr.width() - 24.0);
        painter.text(pos2(nr.min.x + 12.0, nr.center().y), Align2::LEFT_CENTER, text, theme::ui(12.5), c([0xE8, 0xD9, 0xBF]));
    }
}

// Agent and app standard (D30, agent_apps.rs).

/// The file Viewer instance `n` of `agent` shows, relative to the agent directory.
pub(crate) fn agent_shown(core: &mut Core, agent: &str, n: u32) -> Option<PathBuf> {
    let dir = core.agent_dir(agent)?.path.clone();
    state(core, agent, n, dir).shown.clone()
}

/// Shows `rel` in Viewer instance `n` of `agent`.
pub(crate) fn agent_show(core: &mut Core, agent: &str, n: u32, rel: &Path) {
    if let Some(dir) = core.agent_dir(agent).map(|d| d.path.clone()) {
        state(core, agent, n, dir).choose(rel.to_path_buf());
    }
}

#[cfg(test)]
mod tests {
    use super::super::files::tests::TempDir;
    use super::*;

    fn plain(text: &str) -> Span {
        Span { text: text.into(), style: Style::default(), link: None }
    }

    fn text_of(spans: &[Span]) -> String {
        spans.iter().map(|s| s.text.as_str()).collect()
    }

    #[test]
    fn markdown_turns_into_blocks() {
        let md = "# Batch 3 report\n\nMedian is **1.84 s**, *faster* than `B2`. See [the log](logs/run.txt).\n\n\
                  ## Steps\n\n- fixed retries\n- reran batch 3\n  - 412 samples\n\n1. first\n2. second\n\n\
                  ```python\nprint(1)\n```\n\n> quoted\n\n| Batch | Median |\n|:--|--:|\n| B1 | 1.86 |\n| B2 | 1.91 |\n\n---\n";
        let blocks = markdown_blocks(md);
        assert_eq!(blocks[0], Block::Heading(1, vec![plain("Batch 3 report")]));
        let Block::Paragraph(spans) = &blocks[1] else { panic!("{:?}", blocks[1]) };
        assert_eq!(text_of(spans), "Median is 1.84 s, faster than B2. See the log.");
        assert!(spans.iter().any(|s| s.text == "1.84 s" && s.style.bold));
        assert!(spans.iter().any(|s| s.text == "faster" && s.style.italic));
        assert!(spans.iter().any(|s| s.text == "B2" && s.style.code));
        assert!(spans.iter().any(|s| s.text == "the log" && s.link.as_deref() == Some("logs/run.txt")));
        assert_eq!(blocks[2], Block::Heading(2, vec![plain("Steps")]));
        let items: Vec<(usize, &str, String)> = blocks
            .iter()
            .filter_map(|b| match b {
                Block::Item { depth, marker, spans } => Some((*depth, marker.as_str(), text_of(spans))),
                _ => None,
            })
            .collect();
        assert_eq!(
            items,
            [
                (0, "•", "fixed retries".to_string()),
                (0, "•", "reran batch 3".to_string()),
                (1, "•", "412 samples".to_string()),
                (0, "1.", "first".to_string()),
                (0, "2.", "second".to_string())
            ]
        );
        assert!(blocks.contains(&Block::Code { lang: "python".into(), text: "print(1)".into() }));
        assert!(blocks.contains(&Block::Quote(vec![plain("quoted")])));
        let table = blocks.iter().find(|b| matches!(b, Block::Table { .. })).expect("a table");
        let Block::Table { align, head, rows } = table else { unreachable!() };
        assert_eq!(align, &[CellAlign::Left, CellAlign::Right]);
        assert_eq!(head.iter().map(|c| text_of(c)).collect::<Vec<_>>(), ["Batch", "Median"]);
        assert_eq!(rows.len(), 2);
        assert_eq!(text_of(&rows[1][1]), "1.91");
        assert_eq!(blocks.last(), Some(&Block::Rule));
    }

    #[test]
    fn loose_lists_and_task_lists() {
        let blocks = markdown_blocks("- one\n\n  more of one\n\n- [x] done\n");
        let items: Vec<(String, String)> = blocks
            .iter()
            .filter_map(|b| match b {
                Block::Item { marker, spans, .. } => Some((marker.clone(), text_of(spans))),
                _ => None,
            })
            .collect();
        assert_eq!(items, [("•".into(), "one".into()), ("".into(), "more of one".into()), ("•".into(), "[x] done".into())]);
        assert!(markdown_blocks("").is_empty());
    }

    #[test]
    fn hostile_files_stay_within_limits() {
        // A table a mile wide and a mile long.
        let wide = format!("{}|\n{}|\n", "|a".repeat(5000), "|-".repeat(5000));
        let blocks = markdown_blocks(&wide);
        assert!(
            matches!(&blocks[0], Block::Table { head, align, .. } if head.len() == MAX_TABLE_COLUMNS && align.len() == MAX_TABLE_COLUMNS)
        );
        assert!(matches!(&blocks[1], Block::Paragraph(s) if text_of(s).contains("first 500 rows and 50 columns")));
        let long = format!("|a|b|\n|-|-|\n{}", "|x|y|\n".repeat(5000));
        assert!(matches!(&markdown_blocks(&long)[0], Block::Table { rows, .. } if rows.len() == MAX_TABLE_ROWS));
        let many = "para\n\n".repeat(MAX_BLOCKS * 2);
        let blocks = markdown_blocks(&many);
        assert_eq!(blocks.len(), MAX_BLOCKS + 1);
        // Deep nesting parses without trouble.
        assert!(markdown_blocks(&">".repeat(200_000)).len() <= 1);
        assert!(!markdown_blocks(&"*_[".repeat(100_000)).is_empty());

        // CSV: a row of commas, a giant field, and characters of several bytes at the cut.
        let rows = parse_csv(&",".repeat(100_000), ',', MAX_ROWS);
        assert_eq!(rows[0].len(), MAX_COLUMNS);
        let rows = parse_csv(&format!("\"{}\",b\n", "ä".repeat(1_000_000)), ',', MAX_ROWS);
        assert_eq!((rows[0][0].chars().count(), rows[0][1].as_str()), (MAX_FIELD_CHARS, "b"));

        // A giant line of a text file.
        let line = shown_line(&"é\t".repeat(100_000));
        assert!(line.ends_with('…') && line.chars().count() <= MAX_LINE_CHARS * 4 + 1, "{}", line.chars().count());
        assert_eq!(shown_line("a\tb"), "a    b");
    }

    #[test]
    fn hostile_files_draw_without_stalling() {
        let tmp = TempDir::new("viewer-hostile");
        let mut csv = format!("name,{}\n", "c,".repeat(1000));
        csv.push_str(&format!("{},2\n", "ü".repeat(1_000_000)));
        tmp.write("artifacts/wide.csv", csv.as_bytes());
        let md = format!("{}|\n{}|\n{}|\n", "|head".repeat(3000), "|-".repeat(3000), "|cell".repeat(3000));
        tmp.write("artifacts/wide.md", md.as_bytes());
        tmp.write("artifacts/line.txt", "x".repeat(3_000_000).as_bytes());
        let mut t = ViewerTile::new(tmp.0.clone());
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        let screen = Rect::from_min_size(egui::Pos2::ZERO, vec2(900.0, 600.0));
        for file in ["artifacts/wide.csv", "artifacts/wide.md", "artifacts/line.txt"] {
            t.choose(PathBuf::from(file));
            let started = std::time::Instant::now();
            let mut drawn = 0;
            while drawn < 3 && started.elapsed() < std::time::Duration::from_secs(60) {
                let raw = egui::RawInput { screen_rect: Some(screen), ..Default::default() };
                let info = Info { id: "ember".into(), name: "Ember".into(), now: 1.0 };
                let mut output = ctx.run_ui(raw, |ui| {
                    let head = Rect::from_min_size(pos2(10.0, 10.0), vec2(800.0, 38.0));
                    let body = Rect::from_min_max(pos2(11.0, 49.0), pos2(810.0, 590.0));
                    t.show(ui, &info, &[], head, body);
                });
                output.textures_delta.clear();
                if t.doc.as_ref().is_some_and(|d| d.rel == Path::new(file)) {
                    drawn += 1;
                } else {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
            }
            assert_eq!(drawn, 3, "{file} loaded and drew");
            assert!(started.elapsed() < std::time::Duration::from_secs(30), "{file} took {:?}", started.elapsed());
        }
    }

    #[test]
    fn csv_with_quotes() {
        let text = "\u{feff}name,note,n\r\n\"Smith, Jo\",\"said \"\"hi\"\"\",1\n\n\"two\nlines\",,3\nx\"y,plain,\n";
        let rows = parse_csv(text, ',', 100);
        assert_eq!(
            rows,
            vec![vec!["name", "note", "n"], vec!["Smith, Jo", "said \"hi\"", "1"], vec!["two\nlines", "", "3"], vec!["x\"y", "plain", ""],]
        );
        assert_eq!(parse_csv("a,b\n1,2\n3,4\n", ',', 2).len(), 2, "capped");
        assert_eq!(parse_csv("a;b\n\"1;2\";3", ';', 10), vec![vec!["a", "b"], vec!["1;2", "3"]]);
        assert_eq!(sniff_separator("a;b;c\n1,5;2;3"), ';');
        assert_eq!(sniff_separator("\"x;y\",b\n"), ',');
        assert_eq!(sniff_separator("a\tb\n"), '\t');
        assert_eq!(sniff_separator("single\n"), ',');
        assert!(parse_csv("", ',', 10).is_empty());
    }

    #[test]
    fn each_kind_of_file_opens_in_its_app() {
        let tmp = TempDir::new("viewer-route");
        let cases: [(&str, &[u8], Route); 12] = [
            ("plot.png", b"x", Route::Viewer(Kind::Image)),
            ("photo.JPG", b"x", Route::Viewer(Kind::Image)),
            ("anim.gif", b"x", Route::Viewer(Kind::Image)),
            ("report.md", b"# x", Route::Viewer(Kind::Markdown)),
            ("rows.csv", b"a,b", Route::Viewer(Kind::Csv)),
            ("rows.tsv", b"a\tb", Route::Viewer(Kind::Csv)),
            ("page.html", b"<p>", Route::Viewer(Kind::Html)),
            ("paper.pdf", b"%PDF", Route::Viewer(Kind::Pdf)),
            ("archive.bin", b"PK\0\x03", Route::Viewer(Kind::Binary)),
            ("run.py", b"print(1)", Route::Editor),
            ("notes.txt", b"hello", Route::Editor),
            ("Makefile", b"all:", Route::Editor),
        ];
        for (name, bytes, want) in cases {
            let path = tmp.write(name, bytes);
            assert_eq!(route(&path), want, "{name}");
        }
        assert_eq!(route(&tmp.0.join("missing.rs")), Route::Editor);
        assert_eq!(kind_of(&tmp.0.join("notes.txt")), Kind::Text, "text results show in the Viewer as text");
    }

    #[test]
    fn files_load_for_each_kind() {
        let tmp = TempDir::new("viewer-load");
        let png = tmp.0.join("dot.png");
        image::RgbaImage::from_pixel(3, 2, image::Rgba([200, 10, 10, 255])).save(&png).unwrap();
        let loaded = load(&png, 2048);
        assert_eq!(loaded.kind, Kind::Image);
        assert!(matches!(loaded.content, Content::Image { size: [3, 2], note: None, .. }));
        let big = tmp.0.join("big.png");
        image::RgbaImage::new(600, 300).save(&big).unwrap();
        assert!(
            matches!(load(&big, 256).content, Content::Image { size: [600, 300], note: Some(_), ref image } if image.size == [256, 128])
        );
        tmp.write("broken.png", b"not a png");
        assert!(matches!(load(&tmp.0.join("broken.png"), 2048).content, Content::Error(_)));
        tmp.write("r.md", b"# Hi");
        assert!(matches!(load(&tmp.0.join("r.md"), 2048).content, Content::Markdown(ref b) if b.len() == 1));
        tmp.write("t.csv", b"a,b\n1,2\n");
        assert!(matches!(load(&tmp.0.join("t.csv"), 2048).content, Content::Csv { ref rows, note: None } if rows.len() == 2));
        tmp.write("out.log", b"one\n\ttwo\n");
        assert!(matches!(load(&tmp.0.join("out.log"), 2048).content, Content::Text { ref lines, .. } if lines[1] == "    two"));
        assert!(matches!(load(&tmp.0.join("gone.md"), 2048).content, Content::Error(ref e) if e.contains("gone")));
    }

    #[test]
    fn links_and_urls() {
        assert_eq!(resolve_link(Path::new("artifacts"), "plot.png"), Some(PathBuf::from("artifacts/plot.png")));
        assert_eq!(resolve_link(Path::new("artifacts/a"), "../b.md"), Some(PathBuf::from("artifacts/b.md")));
        assert_eq!(resolve_link(Path::new(""), "../../etc/passwd"), None);
        assert_eq!(resolve_link(Path::new(""), "/etc/passwd"), None);
        assert_eq!(percent_decode("my%20notes.md"), "my notes.md");
        assert_eq!(file_url(Path::new("/tmp/a b/r#1.html")), "file:///tmp/a%20b/r%231.html");
    }

    #[test]
    fn new_results_wait_in_the_head() {
        let tmp = TempDir::new("viewer-new");
        tmp.write("artifacts/old.md", b"# Old");
        let mut t = ViewerTile::new(tmp.0.clone());
        let info = Info { id: "ember".into(), name: "Ember".into(), now: 0.0 };
        t.watch(&info, &[]);
        assert_eq!(t.shown, Some(PathBuf::from("artifacts/old.md")), "the newest result shows when nothing is chosen");
        tmp.write("artifacts/new.png", b"x");
        t.watch(&Info { now: 5.0, ..info }, &[]);
        assert_eq!(t.shown, Some(PathBuf::from("artifacts/old.md")), "no switch without asking");
        assert_eq!(t.fresh, Some(PathBuf::from("artifacts/new.png")));
        t.choose(PathBuf::from("artifacts/new.png"));
        assert_eq!(t.fresh, None);

        // A patched report outside artifacts/ counts as a result too, code does not.
        tmp.write("report.md", b"# R");
        tmp.write("run.py", b"x");
        let info = Info { id: "ember".into(), name: "Ember".into(), now: 6.0 };
        let patch = |at: f64, s: &str| ActionEntry { at, tool: "editor.patch".into(), summary: s.into(), target: None };
        t.watch(&info, &[patch(6.0, "run.py +1 -1")]);
        assert_eq!(t.fresh, None);
        t.watch(&info, &[patch(6.0, "run.py +1 -1"), patch(7.0, "report.md")]);
        assert_eq!(t.fresh, Some(PathBuf::from("report.md")));
    }

    #[test]
    fn the_tile_draws_without_a_window() {
        let tmp = TempDir::new("viewer-draw");
        let md = "# Report\n\nSee [plot](plot.png).\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n```\ncode\n```\n";
        tmp.write("artifacts/report.md", md.as_bytes());
        image::RgbaImage::new(40, 20).save(tmp.0.join("artifacts/plot.png")).unwrap();
        tmp.write("artifacts/rows.csv", b"a,b\n1,2\n");
        tmp.write("artifacts/page.html", b"<p>");
        tmp.write("artifacts/out.txt", b"text");
        let mut t = ViewerTile::new(tmp.0.clone());
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        let screen = Rect::from_min_size(egui::Pos2::ZERO, vec2(900.0, 600.0));
        for file in ["artifacts/report.md", "artifacts/plot.png", "artifacts/rows.csv", "artifacts/page.html", "artifacts/out.txt"] {
            t.choose(PathBuf::from(file));
            for frame in 0..40 {
                let raw = egui::RawInput { screen_rect: Some(screen), ..Default::default() };
                let info = Info { id: "ember".into(), name: "Ember".into(), now: frame as f64 * 0.05 };
                let mut output = ctx.run_ui(raw, |ui| {
                    let head = Rect::from_min_size(pos2(10.0, 10.0), vec2(800.0, 38.0));
                    let body = Rect::from_min_max(pos2(11.0, 49.0), pos2(810.0, 590.0));
                    t.show(ui, &info, &[], head, body);
                });
                output.textures_delta.clear();
                if t.doc.as_ref().is_some_and(|d| d.rel == Path::new(file)) {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            assert!(t.doc.as_ref().is_some_and(|d| d.rel == Path::new(file)), "{file} loaded");
        }
        // The head with a new result waiting, wide and narrow.
        t.fresh = Some(PathBuf::from("artifacts/plot.png"));
        for width in [800.0, 260.0] {
            let raw = egui::RawInput { screen_rect: Some(screen), ..Default::default() };
            let info = Info { id: "ember".into(), name: "Ember".into(), now: 3.0 };
            let mut output = ctx.run_ui(raw, |ui| {
                let head = Rect::from_min_size(pos2(10.0, 10.0), vec2(width, 38.0));
                let body = Rect::from_min_max(pos2(11.0, 49.0), pos2(10.0 + width, 590.0));
                t.show(ui, &info, &[], head, body);
            });
            output.textures_delta.clear();
        }
        assert_eq!(t.fresh, Some(PathBuf::from("artifacts/plot.png")), "it waits for Show");
    }
}
