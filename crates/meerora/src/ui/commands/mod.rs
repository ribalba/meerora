//! The command box: Alt+Space (or `[commands] key` in `meerora.toml`)
//! opens a centred field with results over any view, like Alfred on the
//! Mac. It starts apps, opens agents, does math, asks the assistant agent,
//! searches the web, goes to meerora's places and runs the user's own
//! keywords (`commands.toml`) and scripts (`commands/*.js`).
//!
//! - providers.rs turns the query into rows that say what Enter and
//!   Shift+Enter would do. This file carries that out.
//! - Nothing slow runs on the UI thread. The installed apps and the files
//!   load on a thread, each script runs on a thread of its own
//!   (scripts.rs), and `??` asks the daemon (Command::Ask), whose answer
//!   streams back as Event::Answer.
//! - Script and `commands.toml` actions are data. meerora runs them only
//!   when the user picks the row, and the row says what it runs (D8).
//!
//! app.rs catches the key before egui and the app windows see it, so it
//! works while one of your apps has the keyboard. It calls [`toggle`].

pub mod calc;
pub mod config;
pub mod fuzzy;
pub mod providers;
pub mod scripts;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use egui::{Align2, Color32, CornerRadius, Id, Key, Modifiers, Order, Pos2, Rect, Sense, Shape, Stroke, StrokeKind, Ui, pos2, vec2};
use meerora_protocol::{Command, Event, RuntimeKind};

use self::config::{BoxSettings, Entry, Hotkey};
use self::providers::{Act, AgentRef, Answer, Glyph, Input, Place, Row, ScriptView, possessive};
use self::scripts::Scripts;
use super::{AppTab, View};
use crate::desktop_apps::DesktopApp;
use crate::engine::Core;
use crate::theme::{self, c, ca};

/// What the box ran, row id to unix seconds, in the data dir.
const HISTORY_FILE: &str = "commands-recent.json";
/// The launcher's recently used apps (desktop_apps.rs), read for ranking.
const RECENT_APPS_FILE: &str = "recent-apps.json";
const BOX_W: f32 = 720.0;
const INPUT_H: f32 = 62.0;
const ROW_H: f32 = 54.0;
const FOOTER_H: f32 = 30.0;
const VISIBLE: usize = 8;
/// How long loaded files and apps count as fresh when the box opens again.
const FRESH: Duration = Duration::from_secs(5);
const WATCH_EVERY: Duration = Duration::from_secs(1);

fn input_id() -> Id {
    Id::new("command-box-input")
}

/// The state of the box, in `UiState`.
#[derive(Default)]
pub struct CommandBox {
    pub open: bool,
    pub query: String,
    pub selected: usize,
    scroll: usize,
    /// The agent picked with Ctrl+Tab while the box is open.
    target: Option<String>,
    /// The agent whose view showed last, where apps start from the space.
    last_agent: Option<String>,
    loaded: Option<Loaded>,
    loading: Option<Receiver<Loaded>>,
    /// Load again with the next frame.
    want_load: bool,
    /// A load wrote the example files when they were missing.
    examples_checked: bool,
    last_watch: Option<Instant>,
    scripts: Option<Scripts>,
    answer: Option<Answer>,
    ask_seq: u64,
    focus: bool,
    cursor_to_end: bool,
    /// The query of the last frame, to notice typing.
    shown_query: String,
    /// The rows of the last frame.
    pub rows: Vec<Row>,
    /// What the rows were computed from, so they are computed again only
    /// when something changed.
    rows_from: Option<Inputs>,
    /// Snapshots and tests write nothing into the data dir.
    pub read_only: bool,
}

/// What loads on a thread: the settings, the keywords, the installed apps
/// and the recently used lists.
struct Loaded {
    at: Instant,
    settings: BoxSettings,
    entries: Vec<Entry>,
    errors: Vec<String>,
    apps: Vec<DesktopApp>,
    app_recent: HashMap<String, u64>,
    history: HashMap<String, u64>,
    stamp: Stamp,
}

/// What the rows depend on, cheap to compare every frame.
#[derive(Clone, Debug, PartialEq)]
struct Inputs {
    query: String,
    target: Option<String>,
    loaded: Option<Instant>,
    history: usize,
    scripts: u64,
    answer: Option<(String, usize, bool)>,
    daemon: bool,
    agents: usize,
}

/// The modification times of the files the box reads.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Stamp {
    commands: Option<SystemTime>,
    settings: Option<SystemTime>,
}

impl Stamp {
    fn of(data_dir: &Path) -> Self {
        let time = |name: &str| std::fs::metadata(data_dir.join(name)).and_then(|m| m.modified()).ok();
        Stamp { commands: time(config::COMMANDS_FILE), settings: time("meerora.toml") }
    }
}

fn load(data_dir: &Path, examples: bool) -> Loaded {
    if examples {
        for path in config::ensure_examples(data_dir) {
            tracing::info!("wrote the example {}", path.display());
        }
    }
    let settings = BoxSettings::load(data_dir);
    let (entries, entry_errors) = config::load_entries(data_dir);
    let errors = settings.errors.iter().cloned().chain(entry_errors).collect();
    let read_map = |name: &str| -> HashMap<String, u64> {
        std::fs::read_to_string(data_dir.join(name)).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
    };
    Loaded {
        at: Instant::now(),
        apps: crate::desktop_apps::scan(&crate::desktop_apps::search_dirs()),
        app_recent: read_map(RECENT_APPS_FILE),
        history: read_map(HISTORY_FILE),
        settings,
        entries,
        errors,
        stamp: Stamp::of(data_dir),
    }
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

impl CommandBox {
    /// Starts loading on a thread, keeping what is loaded until it is done.
    fn start_load(&mut self, data_dir: &Path) {
        if self.loading.is_some() {
            return;
        }
        let examples = !self.read_only && self.open && !self.examples_checked;
        self.examples_checked |= examples;
        let (tx, rx) = channel();
        let dir = data_dir.to_path_buf();
        let spawned = std::thread::Builder::new().name("command-box-load".into()).spawn(move || {
            let _ = tx.send(load(&dir, examples));
        });
        match spawned {
            Ok(_) => self.loading = Some(rx),
            Err(e) => tracing::warn!("cannot start the command box loader ({e})"),
        }
    }

    /// Takes a finished load. Returns true when one arrived.
    fn poll_load(&mut self) -> bool {
        let Some(rx) = &self.loading else { return false };
        match rx.try_recv() {
            Ok(loaded) => {
                self.loaded = Some(loaded);
                self.loading = None;
                true
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => false,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.loading = None;
                false
            }
        }
    }

    /// Loads in place, for snapshots and tests.
    fn load_now(&mut self, data_dir: &Path) {
        let examples = !self.read_only && !self.examples_checked;
        self.examples_checked |= examples;
        self.loaded = Some(load(data_dir, examples));
    }

    /// The key that opens the box, Alt+Space until `meerora.toml` is read.
    pub fn hotkey(&self) -> Hotkey {
        self.loaded.as_ref().map_or_else(|| BoxSettings::default().key, |l| l.settings.key)
    }

    fn remember(&mut self, data_dir: &Path, id: &str) {
        if id.is_empty() || self.read_only {
            return;
        }
        let Some(loaded) = self.loaded.as_mut() else { return };
        loaded.history.insert(id.to_string(), now_secs());
        // Keep the file small: the 200 newest.
        if loaded.history.len() > 200 {
            let mut items: Vec<(String, u64)> = loaded.history.drain().collect();
            items.sort_by_key(|(_, t)| std::cmp::Reverse(*t));
            items.truncate(200);
            loaded.history = items.into_iter().collect();
        }
        let text = serde_json::to_string_pretty(&loaded.history).unwrap_or_default();
        let path = data_dir.join(HISTORY_FILE);
        let _ = std::thread::Builder::new().name("command-box-history".into()).spawn(move || {
            if let Err(e) = std::fs::write(&path, text) {
                tracing::warn!("cannot write {} ({e})", path.display());
            }
        });
    }
}

/// Whether a key event is the box's key. app.rs asks before egui sees it.
pub fn is_hotkey(core: &Core, code: winit::keyboard::KeyCode, mods: winit::keyboard::ModifiersState) -> bool {
    core.ui.commands.hotkey().matches(code, mods)
}

/// Opens the box, or closes it when it is open.
pub fn toggle(core: &mut Core) {
    if core.ui.commands.open { close(core) } else { open(core) }
}

pub fn open(core: &mut Core) {
    let data_dir = core.assets.data_dir.clone();
    let b = &mut core.ui.commands;
    b.open = true;
    b.focus = true;
    b.selected = 0;
    b.scroll = 0;
    b.target = None;
    b.want_load |= b.loaded.as_ref().is_none_or(|l| l.at.elapsed() > FRESH);
    b.scripts.get_or_insert_with(|| Scripts::new(&data_dir)).scan(true);
    // Keys come to meerora again, not to one of your apps (app.rs).
    core.desktops.focus(None);
}

pub fn close(core: &mut Core) {
    let b = &mut core.ui.commands;
    b.open = false;
    b.query.clear();
    b.shown_query.clear();
    b.target = None;
    b.rows.clear();
    b.rows_from = None;
    if let Some(answer) = b.answer.take()
        && !answer.done
        && let Some(link) = &core.link
    {
        link.send(&Command::CancelAsk { request: answer.request });
    }
}

/// An answer of the daemon to `??`. Returns true for every Answer, which
/// only the box asks for.
pub fn on_event(core: &mut Core, event: &Event) -> bool {
    let Event::Answer { request, text, done, error } = event else { return false };
    if let Some(a) = core.ui.commands.answer.as_mut().filter(|a| a.request == *request) {
        a.text = text.clone();
        a.done = *done;
        a.error = error.clone();
    }
    true
}

/// Agents as the providers see them.
fn agent_refs(core: &Core) -> Vec<AgentRef> {
    core.world
        .agents
        .iter()
        .map(|a| AgentRef {
            id: a.id.clone(),
            name: a.name.clone(),
            color: a.color,
            mock: core.agent_dir(&a.id).and_then(|d| d.config.runtime.as_ref()).is_none_or(|r| r.kind == RuntimeKind::Mock),
        })
        .collect()
}

/// Where apps start and pages open: the agent picked with Ctrl+Tab, else
/// the agent whose view is open, else the one viewed last, else the first
/// real agent.
fn target_id(core: &Core, agents: &[AgentRef]) -> Option<String> {
    let b = &core.ui.commands;
    let exists = |id: &str| agents.iter().any(|a| a.id == id);
    b.target
        .clone()
        .filter(|t| exists(t))
        .or_else(|| match &core.ui.view {
            View::Agent(id) => Some(id.clone()),
            _ => None,
        })
        .or_else(|| b.last_agent.clone().filter(|t| exists(t)))
        .or_else(|| agents.iter().find(|a| !a.mock).or(agents.first()).map(|a| a.id.clone()))
}

fn inputs(core: &Core) -> Inputs {
    let b = &core.ui.commands;
    let agents = agent_refs(core);
    Inputs {
        query: b.query.clone(),
        target: target_id(core, &agents),
        loaded: b.loaded.as_ref().map(|l| l.at),
        history: b.loaded.as_ref().map_or(0, |l| l.history.values().copied().max().unwrap_or(0) as usize + l.history.len()),
        scripts: b.scripts.as_ref().map_or(0, |s| s.generation),
        answer: b.answer.as_ref().map(|a| (a.request.clone(), a.text.len(), a.done)),
        daemon: core.ui.daemon_connected,
        agents: agents.len(),
    }
}

/// The rows for the current query.
fn compute_rows(core: &Core) -> Vec<Row> {
    let b = &core.ui.commands;
    let agents = agent_refs(core);
    let target = target_id(core, &agents);
    let default_settings = BoxSettings::default();
    let empty = HashMap::new();
    let (settings, entries, errors, apps, app_recent, history) = match &b.loaded {
        Some(l) => (&l.settings, &l.entries[..], &l.errors[..], &l.apps[..], &l.app_recent, &l.history),
        None => (&default_settings, &[][..], &[][..], &[][..], &empty, &empty),
    };
    let scripts: Vec<ScriptView> = b
        .scripts
        .iter()
        .flat_map(|s| s.workers())
        .map(|w| ScriptView { name: &w.name, meta: w.meta.as_ref(), error: w.error.as_deref(), answer: w.answer.as_ref() })
        .collect();
    let input = Input {
        query: &b.query,
        agents: &agents,
        target: target.as_deref().and_then(|t| agents.iter().find(|a| a.id == t)),
        settings,
        entries,
        errors,
        apps,
        app_recent,
        history,
        scripts,
        answer: b.answer.as_ref(),
        daemon: core.ui.daemon_connected,
        now: now_secs(),
    };
    providers::rows(&input)
}

/// The keys the box takes from everything else while it is open.
#[derive(Default)]
struct Keys {
    up: bool,
    down: bool,
    page_up: bool,
    page_down: bool,
    enter: bool,
    shift_enter: bool,
    tab: bool,
    ctrl_tab: bool,
    ctrl_shift_tab: bool,
    escape: bool,
}

fn take_keys(ui: &mut Ui) -> Keys {
    ui.input_mut(|i| {
        // Shift and Ctrl+Shift first, since the plain patterns would match them too.
        let shift_enter = i.consume_key(Modifiers::SHIFT, Key::Enter);
        let ctrl_shift_tab = i.consume_key(Modifiers::CTRL | Modifiers::SHIFT, Key::Tab);
        let ctrl_tab = i.consume_key(Modifiers::CTRL, Key::Tab);
        Keys {
            up: i.consume_key(Modifiers::NONE, Key::ArrowUp),
            down: i.consume_key(Modifiers::NONE, Key::ArrowDown),
            page_up: i.consume_key(Modifiers::NONE, Key::PageUp),
            page_down: i.consume_key(Modifiers::NONE, Key::PageDown),
            enter: i.consume_key(Modifiers::NONE, Key::Enter),
            shift_enter,
            tab: i.consume_key(Modifiers::NONE, Key::Tab) | i.consume_key(Modifiers::SHIFT, Key::Tab),
            ctrl_tab,
            ctrl_shift_tab,
            escape: i.consume_key(Modifiers::NONE, Key::Escape),
        }
    })
}

/// Every frame, before the views: follows the viewed agent, loads, and
/// while the box is open takes its keys and draws it on top of everything.
pub fn draw(ui: &mut Ui, core: &mut Core) {
    if let View::Agent(id) = &core.ui.view {
        core.ui.commands.last_agent = Some(id.clone());
    }
    let data_dir = core.assets.data_dir.clone();
    let b = &mut core.ui.commands;
    let fresh = b.poll_load();
    // The key comes from meerora.toml, so it is read soon after the start.
    // The first open also writes the examples when they are missing.
    let examples = b.open && !b.examples_checked && !b.read_only;
    if b.loading.is_none() && (b.loaded.is_none() || b.want_load || examples) {
        b.want_load = false;
        b.start_load(&data_dir);
    }
    if !b.open {
        return;
    }
    if b.last_watch.is_none_or(|t| t.elapsed() >= WATCH_EVERY) {
        b.last_watch = Some(Instant::now());
        if b.loaded.as_ref().is_some_and(|l| l.stamp != Stamp::of(&data_dir)) {
            b.want_load = true;
        }
    }
    let scripts = b.scripts.get_or_insert_with(|| Scripts::new(&data_dir));
    // A load may have written the example scripts just now.
    scripts.scan(fresh);
    scripts.poll();

    let keys = take_keys(ui);
    if keys.escape {
        close(core);
        return;
    }
    let screen = ui.ctx().content_rect();
    let width = BOX_W.min(screen.width() - 32.0);
    let top = screen.min.y + (screen.height() * 0.16).max(72.0);
    let box_min = pos2(screen.center().x - width / 2.0, top);

    let mut clicked: Option<(usize, bool)> = None;
    let mut dismiss = false;
    egui::Area::new(Id::new("command-box")).fade_in(false).fixed_pos(screen.min).order(Order::Tooltip).show(ui.ctx(), |ui| {
        ui.allocate_exact_size(screen.size(), Sense::hover());
        // The backdrop: a click outside the box closes it.
        let backdrop = ui.interact(screen, Id::new("command-box-backdrop"), Sense::click());
        ui.painter().rect_filled(screen, CornerRadius::ZERO, ca(theme::BG, 140));
        // The panel goes here, below the contents, once its height is known.
        let frame = ui.painter().add(Shape::Noop);

        let field = input_field(ui, core, box_min, width);
        let typed = field.changed() || core.ui.commands.query != core.ui.commands.shown_query;
        if typed {
            let b = &mut core.ui.commands;
            b.shown_query = b.query.clone();
            b.selected = 0;
            b.scroll = 0;
        }
        if keys.ctrl_tab || keys.ctrl_shift_tab {
            cycle_target(core, if keys.ctrl_shift_tab { -1 } else { 1 });
        }
        sync_answer(core);
        ask_scripts(core);
        let inputs = inputs(core);
        if core.ui.commands.rows_from.as_ref() != Some(&inputs) {
            core.ui.commands.rows = compute_rows(core);
            core.ui.commands.rows_from = Some(inputs);
        }

        let b = &mut core.ui.commands;
        let count = b.rows.len();
        if count > 0 {
            let last = count - 1;
            if keys.down {
                b.selected = if b.selected >= last { 0 } else { b.selected + 1 };
            }
            if keys.up {
                b.selected = if b.selected == 0 { last } else { b.selected - 1 };
            }
            if keys.page_down {
                b.selected = (b.selected + VISIBLE).min(last);
            }
            if keys.page_up {
                b.selected = b.selected.saturating_sub(VISIBLE);
            }
            b.selected = b.selected.min(last);
        } else {
            b.selected = 0;
        }
        if keys.tab
            && let Some(text) = b.rows.get(b.selected).and_then(|r| r.complete.clone())
        {
            b.query = text;
            b.cursor_to_end = true;
        }
        let list_top = box_min.y + INPUT_H;
        let (list_h, hit) = draw_rows(ui, core, box_min.x, list_top, width);
        clicked = hit;
        let footer = Rect::from_min_size(pos2(box_min.x, list_top + list_h), vec2(width, FOOTER_H));
        draw_footer(ui, core, footer);
        let whole = Rect::from_min_max(box_min, footer.max);
        draw_frame(ui, frame, whole, list_top);
        if backdrop.clicked() && !ui.input(|i| i.pointer.interact_pos()).is_some_and(|p| whole.contains(p)) {
            dismiss = true;
        }
    });
    if dismiss {
        close(core);
        return;
    }
    let b = &core.ui.commands;
    let chosen = clicked.or_else(|| (keys.enter || keys.shift_enter).then_some((b.selected, keys.shift_enter)));
    if let Some((index, alt)) = chosen
        && let Some(row) = b.rows.get(index).cloned()
    {
        core.ui.commands.selected = index;
        choose(ui.ctx(), core, &row, alt);
    }
}

/// The field at the top of the box with the target agent on its right.
fn input_field(ui: &mut Ui, core: &mut Core, at: Pos2, width: f32) -> egui::Response {
    let rect = Rect::from_min_size(at, vec2(width, INPUT_H));
    let painter = ui.painter().clone();
    // The magnifier.
    let center = pos2(rect.min.x + 30.0, rect.center().y - 1.0);
    painter.circle_stroke(center - vec2(2.0, 2.0), 7.0, Stroke::new(2.0, c(theme::TEXT_2)));
    painter.line_segment([center + vec2(3.0, 3.0), center + vec2(8.5, 8.5)], Stroke::new(2.2, c(theme::TEXT_2)));

    let agents = agent_refs(core);
    let target = target_id(core, &agents).and_then(|t| agents.into_iter().find(|a| a.id == t));
    let mut right = rect.max.x - 16.0;
    if let Some(t) = &target {
        let label = format!("ON {}", t.name.to_uppercase());
        let pill = super::pill(&painter, pos2(right, rect.center().y), &label, t.color);
        let hint = painter.text(
            pos2(pill.min.x - 8.0, rect.center().y),
            Align2::RIGHT_CENTER,
            "Ctrl+Tab",
            theme::mono(10.0),
            c([0x6B, 0x72, 0x82]),
        );
        right = hint.min.x - 12.0;
    }
    let field = Rect::from_min_max(pos2(rect.min.x + 52.0, rect.min.y + 16.0), pos2(right, rect.max.y - 14.0));
    let b = &mut core.ui.commands;
    // A completed keyword puts the cursor at its end, before this frame's typing.
    if b.cursor_to_end {
        b.cursor_to_end = false;
        let mut state = egui::TextEdit::load_state(ui.ctx(), input_id()).unwrap_or_default();
        let end = egui::text::CCursor::new(b.query.chars().count());
        state.cursor.set_char_range(Some(egui::text::CCursorRange::one(end)));
        state.store(ui.ctx(), input_id());
    }
    let edit = egui::TextEdit::singleline(&mut b.query)
        .id(input_id())
        .font(theme::ui(20.0))
        .frame(egui::Frame::NONE)
        .margin(egui::Margin::ZERO)
        .desired_width(field.width())
        .hint_text(egui::RichText::new("Apps, agents, = math, ? ask, g search, @ talk").color(c([0x5B, 0x61, 0x70])));
    let response = ui.put(field, edit);
    // Tab, the arrows and Esc belong to the box, not to egui's focus moves.
    ui.memory_mut(|m| {
        m.set_focus_lock_filter(input_id(), egui::EventFilter { tab: true, horizontal_arrows: true, vertical_arrows: true, escape: true })
    });
    if b.focus || !response.has_focus() {
        response.request_focus();
        b.focus = false;
    }
    response
}

/// Cancels a `??` answer once its question is no longer in the field.
fn sync_answer(core: &mut Core) {
    let b = &mut core.ui.commands;
    let Some(answer) = &b.answer else { return };
    let question = b.query.trim_start().strip_prefix("??").map(str::trim);
    if question == Some(answer.question.as_str()) {
        return;
    }
    let answer = b.answer.take().expect("checked above");
    if !answer.done
        && let Some(link) = &core.link
    {
        link.send(&Command::CancelAsk { request: answer.request });
    }
}

/// Sends the query to the scripts whose keyword starts it.
fn ask_scripts(core: &mut Core) {
    let b = &mut core.ui.commands;
    let q = b.query.trim_start();
    let Some((word, rest)) = q.split_once(char::is_whitespace) else { return };
    if let Some(scripts) = b.scripts.as_mut() {
        scripts.ask(word, rest.trim());
    }
}

fn cycle_target(core: &mut Core, step: isize) {
    let agents = agent_refs(core);
    if agents.is_empty() {
        return;
    }
    let current = target_id(core, &agents);
    let at = agents.iter().position(|a| Some(&a.id) == current.as_ref()).unwrap_or(0) as isize;
    let next = &agents[(at + step).rem_euclid(agents.len() as isize) as usize];
    core.ui.commands.target = Some(next.id.clone());
}

/// The height of a row: one line, or the answer text below the title.
fn row_height(ui: &Ui, row: &Row, width: f32) -> f32 {
    match &row.body {
        Some(body) => {
            let galley = ui.painter().layout(body.clone(), theme::ui(14.5), c(theme::TEXT), width - 90.0);
            let lines = (galley.size().y / 20.0).ceil().clamp(1.0, 14.0);
            ROW_H + 8.0 + lines * 20.0
        }
        None => ROW_H,
    }
}

/// Draws the visible rows and returns their height and a clicked row with
/// whether Shift was held.
fn draw_rows(ui: &mut Ui, core: &mut Core, x: f32, top: f32, width: f32) -> (f32, Option<(usize, bool)>) {
    let rows = std::mem::take(&mut core.ui.commands.rows);
    if rows.is_empty() {
        let loading = core.ui.commands.loaded.is_none();
        let text = if loading { "Loading your apps…" } else { "Type to search" };
        ui.painter().text(pos2(x + 56.0, top + 24.0), Align2::LEFT_CENTER, text, theme::ui(14.0), c(theme::TEXT_2));
        core.ui.commands.rows = rows;
        return (48.0, None);
    }
    let selected = core.ui.commands.selected;
    // Keep the selection in the window of visible rows.
    let b = &mut core.ui.commands;
    if selected < b.scroll {
        b.scroll = selected;
    }
    if selected >= b.scroll + VISIBLE {
        b.scroll = selected + 1 - VISIBLE;
    }
    let max_h = (ui.ctx().content_rect().max.y - top - FOOTER_H - 24.0).max(ROW_H * 2.0);
    let scroll = b.scroll.min(rows.len().saturating_sub(1));
    let mut y = top + 4.0;
    let mut hit = None;
    for (i, row) in rows.iter().enumerate().skip(scroll).take(VISIBLE) {
        let h = row_height(ui, row, width);
        if y + h > top + max_h && i > scroll {
            break;
        }
        let rect = Rect::from_min_size(pos2(x + 8.0, y), vec2(width - 16.0, h - 2.0));
        let response = ui.interact(rect, Id::new(("command-row", i)), Sense::click());
        if response.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        if response.clicked() {
            hit = Some((i, ui.input(|inp| inp.modifiers.shift)));
        }
        draw_row(ui, core, row, rect, i == selected, response.hovered());
        y += h;
    }
    let more = rows.len().saturating_sub(scroll + VISIBLE);
    if more > 0 || scroll > 0 {
        let text = match (scroll, more) {
            (0, m) => format!("{m} more below"),
            (s, 0) => format!("{s} more above"),
            (s, m) => format!("{s} above, {m} below"),
        };
        ui.painter().text(pos2(x + width - 18.0, y + 9.0), Align2::RIGHT_CENTER, text, theme::mono(10.0), c([0x6B, 0x72, 0x82]));
        y += 16.0;
    }
    // The wheel moves the window over the list.
    let list = Rect::from_min_max(pos2(x, top), pos2(x + width, y));
    if ui.rect_contains_pointer(list) {
        let wheel = ui.input(|i| i.smooth_scroll_delta.y);
        let b = &mut core.ui.commands;
        if wheel < -1.0 && b.selected + 1 < rows.len() {
            b.selected += 1;
        } else if wheel > 1.0 && b.selected > 0 {
            b.selected -= 1;
        }
    }
    core.ui.commands.rows = rows;
    (y - top + 4.0, hit)
}

fn draw_row(ui: &Ui, core: &Core, row: &Row, rect: Rect, selected: bool, hovered: bool) {
    let painter = ui.painter();
    let red = [0xE0, 0x67, 0x5A];
    if selected {
        painter.rect_filled(rect, CornerRadius::same(10), c([0x1E, 0x23, 0x2E]));
        let bar = Rect::from_min_size(rect.min + vec2(0.0, 10.0), vec2(3.0, (rect.height() - 20.0).min(34.0)));
        painter.rect_filled(bar, CornerRadius::same(2), c(if row.error { red } else { theme::NEEDS_YOU }));
    } else if hovered {
        painter.rect_filled(rect, CornerRadius::same(10), c([0x17, 0x1B, 0x24]));
    }
    let icon = Rect::from_min_size(pos2(rect.min.x + 12.0, rect.min.y + (ROW_H - 2.0 - 34.0) / 2.0), vec2(34.0, 34.0));
    draw_glyph(painter, core, &row.glyph, icon);
    let text_x = icon.max.x + 14.0;
    let mut right = rect.max.x - 12.0;
    if selected {
        let enter = super::pill(painter, pos2(right, rect.min.y + 18.0), "ENTER", if row.error { red } else { theme::NEEDS_YOU });
        right = enter.min.x - 10.0;
    }
    let title_font = theme::ui_semibold(15.5);
    let title = super::fit(painter, &row.title, title_font.clone(), right - text_x);
    painter.text(pos2(text_x, rect.min.y + 17.0), Align2::LEFT_CENTER, title, title_font, c(if row.error { red } else { theme::TEXT }));
    let sub_font = theme::ui(12.5);
    let sub_w = rect.max.x - 12.0 - text_x;
    match &row.body {
        Some(body) => {
            let galley = painter.layout(body.clone(), theme::ui(14.5), c(theme::TEXT), rect.width() - 90.0);
            let shown = Rect::from_min_size(pos2(text_x, rect.min.y + 32.0), vec2(rect.width() - 90.0, rect.height() - 32.0 - 24.0));
            painter.with_clip_rect(shown).galley(shown.min, galley, c(theme::TEXT));
            let sub = super::fit(painter, &row.subtitle, sub_font.clone(), sub_w);
            painter.text(pos2(text_x, rect.max.y - 13.0), Align2::LEFT_CENTER, sub, sub_font, c(theme::TEXT_2));
        }
        None => {
            let sub = super::fit(painter, &row.subtitle, sub_font.clone(), sub_w);
            painter.text(pos2(text_x, rect.min.y + 36.0), Align2::LEFT_CENTER, sub, sub_font, c(theme::TEXT_2));
        }
    }
}

/// A colour for an app from its name, so each keeps its own.
fn tint(name: &str) -> [u8; 3] {
    const COLORS: [[u8; 3]; 6] =
        [theme::NEEDS_YOU, theme::WORKING, theme::SYSTEM_2, [0x7F, 0xE3, 0xD0], [0xE8, 0x9A, 0x7A], [0xA8, 0xD8, 0x8C]];
    let hash = name.bytes().fold(7u32, |h, b| h.wrapping_mul(31).wrapping_add(u32::from(b)));
    COLORS[hash as usize % COLORS.len()]
}

fn draw_glyph(painter: &egui::Painter, core: &Core, glyph: &Glyph, rect: Rect) {
    let square =
        |fill: Color32| painter.rect(rect, CornerRadius::same(9), fill, Stroke::new(1.0, c([0x2A, 0x30, 0x3D])), StrokeKind::Inside);
    let text = |s: &str, font: egui::FontId, color: [u8; 3]| painter.text(rect.center(), Align2::CENTER_CENTER, s, font, c(color));
    match glyph {
        Glyph::Agent(id) => match core.world.agent(id) {
            Some(a) => {
                square(ca(a.color, 34));
                super::portrait(painter, rect.shrink(1.0), core, a);
            }
            None => {
                square(c(theme::PANEL_2));
                text("@", theme::ui_bold(17.0), theme::TEXT_2);
            }
        },
        Glyph::App(name) => {
            let color = tint(name);
            square(ca(color, 38));
            let letter: String =
                name.chars().find(|c| c.is_alphanumeric()).map(|c| c.to_uppercase().collect()).unwrap_or_else(|| "?".into());
            text(&letter, theme::ui_bold(17.0), color);
        }
        Glyph::Builtin(app) => {
            square(c([0x1A, 0x1F, 0x2A]));
            use meerora_protocol::App;
            let mark = match app {
                App::Chat => "Aa".to_string(),
                App::Editor => "{}".into(),
                App::Terminal => ">_".into(),
                App::Browser => "//".into(),
                App::Files => "[]".into(),
                App::Viewer => "<>".into(),
                App::Changes => "+-".into(),
                App::Activity => "~".into(),
                // Apps added later show the start of their name.
                #[allow(unreachable_patterns)]
                other => AppTab::from_app(*other).title().chars().take(2).collect(),
            };
            text(&mark, theme::mono_medium(13.0), theme::TEXT);
        }
        Glyph::Math => {
            square(ca(theme::WORKING, 34));
            text("=", theme::ui_bold(24.0), theme::WORKING);
        }
        Glyph::Ask => {
            square(ca(theme::SYSTEM, 40));
            text("?", theme::ui_bold(22.0), theme::SYSTEM_2);
        }
        Glyph::Web => {
            square(ca(theme::NEEDS_YOU, 30));
            let center = rect.center();
            let stroke = Stroke::new(1.6, c(theme::NEEDS_YOU));
            painter.circle_stroke(center, 9.0, stroke);
            let meridian: Vec<Pos2> =
                (0..=24).map(|k| k as f32 / 24.0 * std::f32::consts::TAU).map(|t| center + vec2(4.0 * t.cos(), 9.0 * t.sin())).collect();
            painter.add(Shape::line(meridian, stroke));
            painter.line_segment([center - vec2(9.0, 0.0), center + vec2(9.0, 0.0)], stroke);
        }
        Glyph::Place => {
            square(c([0x1A, 0x1F, 0x2A]));
            super::logo(painter, rect.center() + vec2(0.0, 1.0), 18.0);
        }
        Glyph::Command(keyword) => {
            let teal = [0x7F, 0xE3, 0xD0];
            square(ca([0x1F, 0x6F, 0x6B], 90));
            let short: String = keyword.chars().take(3).collect();
            text(&short, theme::mono_medium(if short.chars().count() > 2 { 11.0 } else { 13.0 }), teal);
        }
        Glyph::Script(mark) => {
            let teal = [0x7F, 0xE3, 0xD0];
            square(ca(teal, 26));
            text(mark, theme::ui_bold(if mark.chars().count() > 1 { 13.0 } else { 21.0 }), teal);
        }
        Glyph::Error => {
            square(ca([0xE0, 0x67, 0x5A], 36));
            text("!", theme::ui_bold(18.0), [0xE0, 0x67, 0x5A]);
        }
    }
}

fn draw_footer(ui: &Ui, core: &Core, rect: Rect) {
    let painter = ui.painter();
    super::hline(painter, rect.min.y, rect.min.x + 12.0, rect.max.x - 12.0);
    let b = &core.ui.commands;
    let dim = c([0x6B, 0x72, 0x82]);
    let left = b
        .rows
        .get(b.selected)
        .and_then(|r| r.alt.as_ref().map(|(what, _)| what.clone()))
        .unwrap_or_else(|| "Up and Down select · Enter runs".into());
    let right = format!("Tab completes · Esc or {} closes", b.hotkey().label());
    let right_r = painter.text(pos2(rect.max.x - 16.0, rect.center().y), Align2::RIGHT_CENTER, right, theme::mono(10.5), dim);
    let left = super::fit(painter, &left, theme::mono(10.5), right_r.min.x - rect.min.x - 36.0);
    painter.text(pos2(rect.min.x + 16.0, rect.center().y), Align2::LEFT_CENTER, left, theme::mono(10.5), c(theme::TEXT_2));
}

/// The panel behind the box, into the place `idx` kept below its contents.
fn draw_frame(ui: &Ui, idx: egui::layers::ShapeIdx, rect: Rect, list_top: f32) {
    let shadow = egui::epaint::Shadow { offset: [0, 12], blur: 40, spread: 0, color: Color32::from_black_alpha(150) };
    let frame = Shape::rect_filled(rect, CornerRadius::same(16), c(theme::PANEL));
    let border = Shape::rect_stroke(rect, CornerRadius::same(16), Stroke::new(1.0, c(theme::BORDER)), StrokeKind::Inside);
    let line =
        Shape::line_segment([pos2(rect.min.x + 1.0, list_top), pos2(rect.max.x - 1.0, list_top)], Stroke::new(1.0, c([0x1B, 0x1F, 0x28])));
    ui.painter().set(idx, Shape::Vec(vec![shadow.as_shape(rect, CornerRadius::same(16)).into(), frame, border, line]));
}

/// Runs a row: Enter, or Shift+Enter with `alt`.
fn choose(ctx: &egui::Context, core: &mut Core, row: &Row, alt: bool) {
    let act = if alt { row.alt.as_ref().map(|(_, act)| act.clone()) } else { None }.unwrap_or_else(|| row.act.clone());
    let ran = !matches!(act, Act::Nothing | Act::Fill(_) | Act::AskQuick { .. });
    let close_after = perform(ctx, core, act);
    if ran {
        let data_dir = core.assets.data_dir.clone();
        core.ui.commands.remember(&data_dir, &row.id);
    }
    if close_after {
        close(core);
    }
}

fn agent_name(core: &Core, id: &str) -> String {
    core.world.agent(id).map_or_else(|| id.to_string(), |a| a.name.clone())
}

fn toast(core: &mut Core, text: impl Into<String>, error: bool) {
    let now = core.world.time;
    core.ui.toast(now, text, error);
}

/// Carries out an action. Returns whether the box closes after it.
fn perform(ctx: &egui::Context, core: &mut Core, act: Act) -> bool {
    match act {
        Act::Nothing => false,
        Act::Fill(text) => {
            let b = &mut core.ui.commands;
            b.query = text;
            b.cursor_to_end = true;
            false
        }
        Act::Copy(text) => {
            ctx.copy_text(text.clone());
            let shown: String = text.chars().take(48).collect();
            let more = if text.chars().count() > 48 { "…" } else { "" };
            toast(core, format!("Copied {shown}{more}"), false);
            true
        }
        Act::Notify(text) => {
            toast(core, text, false);
            true
        }
        Act::Launch { app, agent, show } => {
            let cwd = core.agent_dir(&agent).map(|d| d.path.clone());
            let name = agent_name(core, &agent);
            match core.desktops.launch(&agent, &app, cwd.as_deref()) {
                Ok(()) => toast(core, format!("Starting {} on {} desktop", app.name, possessive(&name)), false),
                Err(e) => toast(core, e, true),
            }
            if show {
                super::open_agent(core, &agent);
            }
            true
        }
        Act::Run { agent, program, args, show } => {
            let cwd = core.agent_dir(&agent).map(|d| d.path.clone());
            let name = agent_name(core, &agent);
            let short = Path::new(&program).file_name().map_or(program.clone(), |n| n.to_string_lossy().into_owned());
            match core.desktops.launch_command(&agent, &short, &program, &args, cwd.as_deref()) {
                Ok(()) => toast(core, format!("Starting {short} on {} desktop", possessive(&name)), false),
                Err(e) => toast(core, e, true),
            }
            if show {
                super::open_agent(core, &agent);
            }
            true
        }
        Act::Builtin { agent, app, new } => {
            super::open_agent(core, &agent);
            let tab = if new { super::agent::next_instance(core, &agent, app) } else { super::agent::target(core, &agent, app) };
            super::agent::open_later(core, tab);
            true
        }
        Act::Tab { agent, tab } => {
            super::open_agent(core, &agent);
            super::agent::open_later(core, tab);
            true
        }
        Act::Agent { agent, chat } => {
            super::open_agent(core, &agent);
            if chat {
                super::agent::open_later(core, AppTab::Chat);
            }
            true
        }
        Act::Say { agent, text, open } => {
            let sent = match &core.link {
                Some(link) if core.ui.daemon_connected => {
                    link.send(&Command::Say { agent: agent.clone(), text });
                    true
                }
                _ => false,
            };
            if !sent {
                toast(core, "The agent daemon is not running", true);
                return false;
            }
            if open {
                super::open_agent(core, &agent);
                super::agent::open_later(core, AppTab::Chat);
            } else {
                let name = agent_name(core, &agent);
                toast(core, format!("Sent to {name}"), false);
            }
            true
        }
        Act::Url { agent, url } => {
            super::open_agent(core, &agent);
            super::browser::open_url(core, &agent, &url);
            true
        }
        Act::AskQuick { agent, question } => {
            ask_quick(core, agent, question);
            false
        }
        Act::Go(place) => {
            match place {
                Place::Space => core.ui.view = View::Space,
                Place::Feed => super::feed::open(core),
                Place::Graveyard => super::graveyard::open(core, None),
                Place::Rooms => {
                    core.ui.view = View::Space;
                    super::rooms::open(ctx, core);
                }
                Place::Room(id, _) => match super::rooms::choose(core, &id) {
                    Ok(text) => toast(core, text, false),
                    Err(e) => toast(core, e, true),
                },
                Place::NewAgent => super::open_builder(core),
                Place::Index => super::index::open(core),
            }
            true
        }
        Act::EditCommands { agent } => edit_commands(ctx, core, agent),
    }
}

/// Sends `??`'s question to the daemon. The answer arrives as Event::Answer.
fn ask_quick(core: &mut Core, agent: String, question: String) {
    let Some(link) = core.link.as_ref().filter(|_| core.ui.daemon_connected) else {
        toast(core, "The agent daemon is not running", true);
        return;
    };
    let b = &mut core.ui.commands;
    b.ask_seq += 1;
    let request = format!("box-{}-{}", std::process::id(), b.ask_seq);
    if let Some(old) = b.answer.take()
        && !old.done
    {
        link.send(&Command::CancelAsk { request: old.request });
    }
    link.send(&Command::Ask { request: request.clone(), agent: agent.clone(), question: question.clone() });
    b.answer = Some(Answer { request, agent, question, text: String::new(), done: false, error: None });
}

/// Text editors to open `commands.toml` with, most likely first.
const EDITORS: [&str; 10] = [
    "org.gnome.TextEditor.desktop",
    "org.gnome.gedit.desktop",
    "org.kde.kate.desktop",
    "org.kde.kwrite.desktop",
    "org.xfce.mousepad.desktop",
    "mousepad.desktop",
    "xed.desktop",
    "pluma.desktop",
    "dev.zed.Zed.desktop",
    "code.desktop",
];

/// Opens `commands.toml` in a text editor as a real app on `agent`'s
/// desktop, writing the example first when the file is missing. Without an
/// agent or an editor it copies the path.
fn edit_commands(ctx: &egui::Context, core: &mut Core, agent: Option<String>) -> bool {
    let path: PathBuf = core.assets.data_dir.join(config::COMMANDS_FILE);
    if !path.exists() && !core.ui.commands.read_only {
        config::ensure_examples(&core.assets.data_dir);
    }
    let apps: &[DesktopApp] = core.ui.commands.loaded.as_ref().map_or(&[], |l| &l.apps);
    let editor = EDITORS
        .iter()
        .find_map(|id| apps.iter().find(|a| a.id == *id))
        .or_else(|| apps.iter().find(|a| a.hints.to_lowercase().contains("text editor") || a.name.to_lowercase().contains("text editor")))
        .cloned();
    let path_text = path.display().to_string();
    match (agent, editor) {
        (Some(agent), Some(app)) => {
            let cwd = core.agent_dir(&agent).map(|d| d.path.clone());
            let (program, args) = app.exec.split_first().map(|(p, a)| (p.clone(), a.to_vec())).unwrap_or_default();
            let args: Vec<String> = args.into_iter().chain([path_text.clone()]).collect();
            match core.desktops.launch_command(&agent, &app.name, &program, &args, cwd.as_deref()) {
                Ok(()) => toast(core, format!("Opening commands.toml in {}", app.name), false),
                Err(e) => toast(core, e, true),
            }
            super::open_agent(core, &agent);
        }
        (agent, _) => {
            ctx.copy_text(path_text.clone());
            let why = if agent.is_some() { "No text editor found. " } else { "" };
            toast(core, format!("{why}Copied {path_text}"), false);
        }
    }
    true
}

/// Opens the box with `query` for a snapshot, loading everything in place
/// and waiting for the scripts. `answer` shows a finished `??` answer.
pub fn snapshot(core: &mut Core, query: &str, answer: Option<&str>) -> anyhow::Result<()> {
    let data_dir = core.assets.data_dir.clone();
    core.ui.daemon_connected |= answer.is_some();
    let b = &mut core.ui.commands;
    b.read_only = true;
    b.load_now(&data_dir);
    b.open = true;
    b.focus = true;
    b.query = query.to_string();
    let scripts = b.scripts.get_or_insert_with(|| Scripts::new(&data_dir));
    scripts.scan(true);
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        scripts.poll();
        if scripts.settled() {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let (word, rest) = query.trim_start().split_once(char::is_whitespace).unwrap_or((query, ""));
    scripts.ask(word, rest.trim());
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        scripts.poll();
        if scripts.settled() {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    if let Some(text) = answer {
        let question = query.trim_start().strip_prefix("??").map(str::trim).unwrap_or(query).to_string();
        let agent = b.loaded.as_ref().map_or_else(|| config::DEFAULT_ASSISTANT.to_string(), |l| l.settings.assistant.clone());
        b.answer = Some(Answer { request: "snapshot".into(), agent, question, text: text.replace("\\n", "\n"), done: true, error: None });
    }
    Ok(())
}

#[cfg(test)]
mod tests;
