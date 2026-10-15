//! "Get an external agent" (D25): search the meerora index through the
//! daemon, see each result's picture, owner, welcome text and search terms,
//! watch its figure in a live preview and import it. No screen is designed
//! for this yet (Q14), so it follows the builder (screen 3): the list on the
//! left, the preview on the right.
//!
//! Also the small marks that other views draw for imported agents: the
//! "external" badge with the owner's domain and the privacy note above the
//! chat input.
//!
//! Text shown here never cites decision numbers, the user does not know them.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use egui::{Align2, Color32, CornerRadius, Id, LayerId, Order, Painter, Pos2, Rect, Sense, Stroke, StrokeKind, Ui, pos2, vec2};
use meerora_protocol::{AgentDir, Command, DEFAULT_INDEX_URL, EXTERNAL_TRIANGLES, Event, IndexEntry, ManifestKind, Settings};
use meerora_script::{FigureDesc, Limits};

use super::{ButtonStyle, View, button, fit, label, logo};
use crate::engine::Core;
use crate::preview::{PreviewAnim, PreviewInput};
use crate::theme::{self, c, ca};

/// External agents use the company colours of the brief.
pub const TEAL: [u8; 3] = [0x1F, 0x6F, 0x6B];
pub const MINT: [u8; 3] = [0x7F, 0xE3, 0xD0];
/// Seconds after the last key before the search goes out.
const TYPE_PAUSE: f64 = 0.35;
const ROW_H: f32 = 112.0;

/// One search result.
pub struct Found {
    pub entry: IndexEntry,
    /// Built when the result is first selected. `Err` says why there is no preview.
    figure: Option<Result<FigureDesc, String>>,
    version: u64,
}

pub struct IndexState {
    pub query: String,
    /// The query of the last search sent, and when the user last typed.
    searched: Option<String>,
    typed_at: f64,
    /// Request id of the search in flight.
    pending: Option<String>,
    pub results: Vec<Found>,
    pub error: Option<String>,
    pub selected: Option<usize>,
    /// Manifest id of the import in flight.
    importing: Option<String>,
    pub anim: PreviewAnim,
    anim_since: f64,
    yaw: f32,
    /// Pixel size of the preview stage in the last frame.
    preview_px: (u32, u32),
    pictures: Pictures,
    /// Base URL of the index, for the header.
    index_url: String,
}

impl IndexState {
    fn new(now: f64, index_url: String) -> Self {
        Self {
            query: String::new(),
            searched: None,
            typed_at: now,
            pending: None,
            results: Vec::new(),
            error: None,
            selected: None,
            importing: None,
            anim: PreviewAnim::Working,
            anim_since: now,
            yaw: 0.0,
            preview_px: (0, 0),
            pictures: Pictures::new(),
            index_url,
        }
    }

    fn selected(&self) -> Option<&Found> {
        self.selected.and_then(|i| self.results.get(i))
    }

    /// Builds the figure of the selected result, once.
    fn build_selected(&mut self) {
        let Some(found) = self.selected.and_then(|i| self.results.get_mut(i)) else { return };
        if found.figure.is_some() {
            return;
        }
        // The same limit as the import, so a figure that previews also imports.
        let limits = Limits { hard_triangle_cap: EXTERNAL_TRIANGLES, ..Limits::default() };
        found.figure = Some(match &found.entry.figure_script {
            Some(script) => meerora_script::build_figure(script, "figure.js", &limits).map_err(|e| e.to_string()),
            None => Err("The figure could not be fetched or did not match its checksum.".into()),
        });
        found.version = next_version();
    }
}

/// Preview versions of this screen. They never meet the builder's, which count up from 1.
fn next_version() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1 << 62);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// Opens the search screen and lists the whole index.
pub fn open(core: &mut Core) {
    let url = Settings::load(&core.assets.data_dir).ok().and_then(|s| s.index.url).unwrap_or_else(|| DEFAULT_INDEX_URL.into());
    let mut state = IndexState::new(core.world.time, url);
    search(core, &mut state, String::new());
    core.ui.index = Some(state);
    core.ui.view = View::Index;
    // The Follow and Befriend buttons show the relations (D27).
    super::feed::refresh_relations(core);
}

fn search(core: &Core, state: &mut IndexState, query: String) {
    state.searched = Some(query.clone());
    match &core.link {
        Some(link) if core.ui.daemon_connected => {
            let request = format!("search-{}", next_version() & 0xFFFF_FFFF);
            link.send(&Command::IndexSearch { request: request.clone(), query });
            state.pending = Some(request);
            state.error = None;
        }
        _ => state.error = Some("The agent daemon is not running, so the index cannot be searched.".into()),
    }
}

/// Takes the answers of the daemon. Returns true for the events this screen owns.
pub fn on_event(core: &mut Core, event: &Event) -> bool {
    match event {
        Event::IndexResults { request, results, error } => {
            if let Some(state) = core.ui.index.as_mut()
                && state.pending.as_ref().is_none_or(|p| p == request)
            {
                state.pending = None;
                state.error = error.clone();
                show_results(state, results.clone());
            }
            true
        }
        Event::Imported { manifest, agent, error } => {
            if let Some(state) = core.ui.index.as_mut()
                && state.importing.as_deref() == Some(manifest.as_str())
            {
                state.importing = None;
            }
            let name = core
                .ui
                .index
                .as_ref()
                .and_then(|s| s.results.iter().find(|f| f.entry.manifest.id == *manifest))
                .map_or(manifest.clone(), |f| f.entry.manifest.name.clone());
            match (agent, error) {
                (Some(id), _) => {
                    let known = core.world.agent_index(id).is_some();
                    match meerora_protocol::load_agent(&core.assets.data_dir.join("agents").join(id)) {
                        Ok(dir) => core.adopt_agent(dir),
                        Err(e) => core.ui.toast(core.world.time, e, true),
                    }
                    let what = if known { "was updated" } else { "moved into your space and waits at the door" };
                    core.ui.toast(core.world.time, format!("{name} {what}"), false);
                    if core.ui.view == View::Index {
                        core.ui.view = View::Space;
                    }
                }
                (None, error) => {
                    let error = error.clone().unwrap_or_else(|| "the import failed".into());
                    if let Some(state) = core.ui.index.as_mut() {
                        state.error = Some(format!("{name} could not be imported: {error}"));
                    }
                    core.ui.toast(core.world.time, format!("{name} could not be imported: {error}"), true);
                }
            }
            true
        }
        _ => false,
    }
}

/// Replaces the results and keeps the selection on the same manifest.
pub fn show_results(state: &mut IndexState, results: Vec<IndexEntry>) {
    let keep = state.selected().map(|f| f.entry.manifest.id.clone());
    state.results = results.into_iter().map(|entry| Found { entry, figure: None, version: 0 }).collect();
    state.selected =
        keep.and_then(|id| state.results.iter().position(|f| f.entry.manifest.id == id)).or((!state.results.is_empty()).then_some(0));
    for found in &state.results {
        if let Some(url) = &found.entry.manifest.picture {
            state.pictures.request(url);
        }
        // A person's photos for the preview (D26).
        for photo in found.entry.profile.iter().flat_map(|p| &p.photos) {
            state.pictures.request(&photo.url);
        }
    }
    state.build_selected();
}

/// Waits until every picture loaded or failed, for snapshots.
pub fn wait_for_pictures(core: &mut Core, timeout: Duration) {
    if let Some(state) = core.ui.index.as_mut() {
        state.pictures.wait(timeout);
    }
}

/// Selects a result, for snapshots.
pub fn select(core: &mut Core, i: usize) {
    if let Some(state) = core.ui.index.as_mut()
        && i < state.results.len()
    {
        state.selected = Some(i);
        state.build_selected();
    }
}

/// Draws the selected figure into the preview texture.
pub fn render_preview(core: &mut Core, device: &wgpu::Device, queue: &wgpu::Queue, encoder: &mut wgpu::CommandEncoder) {
    let Core { ui, preview, egui_renderer, world, .. } = core;
    let Some(state) = ui.index.as_ref() else { return };
    let Some(found) = state.selected() else { return };
    let Some(Ok(figure)) = &found.figure else { return };
    static NO_PARAMS: std::sync::LazyLock<HashMap<String, serde_json::Value>> = std::sync::LazyLock::new(HashMap::new);
    preview.set_figure(device, found.version, figure);
    let input = PreviewInput { figure, anim: state.anim, time: (world.time - state.anim_since) as f32, yaw: state.yaw, params: &NO_PARAMS };
    preview.render(device, queue, encoder, egui_renderer, state.preview_px, &input);
}

pub fn draw(ui: &mut Ui, core: &mut Core) {
    let Some(mut state) = core.ui.index.take() else {
        core.ui.view = View::Space;
        return;
    };
    state.pictures.poll(ui.ctx());
    let now = core.world.time;
    if state.searched.as_deref() != Some(state.query.trim()) && now - state.typed_at > TYPE_PAUSE {
        let query = state.query.trim().to_string();
        search(core, &mut state, query);
    }
    let screen = ui.ctx().content_rect();
    ui.ctx().layer_painter(LayerId::background()).rect_filled(screen, CornerRadius::ZERO, c(theme::BG));
    let mut back = false;
    let mut import = false;
    egui::Area::new(Id::new("index")).fade_in(false).fixed_pos(screen.min).order(Order::Middle).show(ui.ctx(), |ui| {
        ui.allocate_exact_size(screen.size(), Sense::hover());
        (back, import) = header(ui, core, &state, screen);
        // Follow and Befriend left of Import (D27).
        if let Some(found) = state.selected() {
            super::feed::index_buttons(ui, core, &found.entry, screen.max.x - 24.0 - 170.0 - 12.0);
        }
        let body = Rect::from_min_max(pos2(24.0, 92.0), pos2(screen.max.x - 24.0, screen.max.y - 24.0));
        let left = Rect::from_min_size(body.min, vec2(600.0, body.height()));
        let right = Rect::from_min_max(pos2(left.max.x + 16.0, body.min.y), body.max);
        results_pane(ui, core, &mut state, left);
        detail_pane(ui, core, &mut state, right);
    });
    if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
        back = true;
    }
    if import && let Some(found) = state.selected() {
        let id = found.entry.manifest.id.clone();
        match &core.link {
            Some(link) if core.ui.daemon_connected => {
                link.send(&Command::ImportAgent { manifest: id.clone() });
                state.importing = Some(id);
                state.error = None;
            }
            _ => state.error = Some("The agent daemon is not running, so nothing can be imported.".into()),
        }
    }
    if state.pending.is_some() || state.importing.is_some() {
        ui.ctx().request_repaint_after(Duration::from_millis(100));
    }
    if back {
        core.ui.view = View::Space;
    }
    core.ui.index = Some(state);
}

/// The agent already imported from this manifest, if any.
fn imported_as(core: &Core, manifest: &str) -> Option<String> {
    core.world
        .agents
        .iter()
        .find(|a| core.agent_dir(&a.id).and_then(|d| d.config.external.as_ref()).is_some_and(|e| e.manifest == manifest))
        .map(|a| a.id.clone())
}

/// Returns (back, import).
fn header(ui: &mut Ui, core: &Core, state: &IndexState, screen: Rect) -> (bool, bool) {
    let painter = ui.painter();
    super::hline(painter, 72.0, 0.0, screen.max.x);
    let back_r = Rect::from_min_size(pos2(24.0, 14.0), vec2(132.0, 44.0));
    let back = button(ui, back_r, "Your space", ButtonStyle::Quiet).clicked();
    let painter = ui.painter();
    logo(painter, pos2(back_r.max.x + 26.0, 36.0), 18.0);
    painter.text(pos2(back_r.max.x + 46.0, 26.0), Align2::LEFT_CENTER, "Get an external agent", theme::ui_bold(18.0), c(theme::TEXT));
    let host = state.index_url.split("://").nth(1).unwrap_or(&state.index_url).trim_end_matches('/');
    painter.text(
        pos2(back_r.max.x + 46.0, 48.0),
        Align2::LEFT_CENTER,
        format!("MEERORA INDEX · {}", host.to_uppercase()),
        theme::mono(11.0),
        c(theme::TEXT_2),
    );

    let next = Rect::from_min_size(pos2(screen.max.x - 24.0 - 170.0, 14.0), vec2(170.0, 44.0));
    let Some(found) = state.selected() else {
        disabled(ui, next, "Import");
        return (back, false);
    };
    let name = &found.entry.manifest.name;
    let label = if state.importing.is_some() {
        "Importing…".to_string()
    } else if imported_as(core, &found.entry.manifest.id).is_some() {
        format!("Update {name}")
    } else {
        format!("Import {name}")
    };
    let label = fit(ui.painter(), &label, theme::ui_bold(14.5), next.width() - 24.0);
    let ready = state.importing.is_none() && matches!(found.figure, Some(Ok(_)));
    if !ready {
        disabled(ui, next, &label);
        return (back, false);
    }
    let import = button(ui, next, &label, ButtonStyle::Primary).clicked();
    (back, import)
}

fn disabled(ui: &Ui, rect: Rect, text: &str) {
    let painter = ui.painter();
    painter.rect(rect, CornerRadius::same(10), c([0x1A, 0x1E, 0x28]), Stroke::NONE, StrokeKind::Inside);
    painter.text(rect.center(), Align2::CENTER_CENTER, text, theme::ui_bold(14.5), c([0x5B, 0x61, 0x70]));
}

fn results_pane(ui: &mut Ui, core: &mut Core, state: &mut IndexState, rect: Rect) {
    let painter = ui.painter();
    super::panel(painter, rect, 18);
    painter.text(rect.min + vec2(20.0, 20.0), Align2::LEFT_TOP, "Find an agent", theme::ui_bold(22.0), c(theme::TEXT));
    let sub = painter.layout(
        "Companies, institutions and people publish their agents in the meerora index. Import one and it moves into your space.".into(),
        theme::ui(14.0),
        c(theme::TEXT_2),
        rect.width() - 40.0,
    );
    painter.galley(rect.min + vec2(20.0, 50.0), sub, c(theme::TEXT_2));

    // The search field.
    let field = Rect::from_min_size(pos2(rect.min.x + 20.0, rect.min.y + 100.0), vec2(rect.width() - 40.0, 48.0));
    painter.rect(field, CornerRadius::same(14), c([0x16, 0x1A, 0x23]), Stroke::new(1.0, c([0x2A, 0x30, 0x3D])), StrokeKind::Inside);
    let glass = pos2(field.min.x + 22.0, field.center().y - 1.0);
    painter.circle_stroke(glass, 6.5, Stroke::new(1.6, c(theme::TEXT_2)));
    painter.line_segment([glass + vec2(4.8, 4.8), glass + vec2(9.5, 9.5)], Stroke::new(1.8, c(theme::TEXT_2)));
    let mut field_ui =
        ui.new_child(egui::UiBuilder::new().max_rect(Rect::from_min_max(field.min + vec2(42.0, 14.0), field.max - vec2(14.0, 10.0))));
    let before = state.query.clone();
    let resp = field_ui.add(
        egui::TextEdit::singleline(&mut state.query)
            .id(Id::new("index-search"))
            .frame(egui::Frame::NONE)
            .hint_text(egui::RichText::new("Search people, companies and institutions").color(c([0x6B, 0x72, 0x82])))
            .font(theme::ui(15.0))
            .desired_width(field.width() - 60.0),
    );
    if state.query != before {
        state.typed_at = core.world.time;
    }
    if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
        let query = state.query.trim().to_string();
        search(core, state, query);
    }

    // What the list shows.
    let status_y = field.max.y + 16.0;
    let painter = ui.painter();
    let (status, color) = if let Some(e) = &state.error {
        (e.clone(), [0xE0, 0x67, 0x5A])
    } else if state.pending.is_some() {
        ("SEARCHING THE INDEX…".to_string(), theme::WORKING)
    } else {
        let what = if state.searched.as_deref().is_none_or(str::is_empty) { "IN THE INDEX".to_string() } else { "FOUND".to_string() };
        match state.results.len() {
            1 => (format!("1 AGENT {what}"), theme::TEXT_2),
            n => (format!("{n} AGENTS {what}"), theme::TEXT_2),
        }
    };
    let galley = painter.layout(status, theme::mono(11.0), c(color), rect.width() - 40.0);
    let status_h = galley.size().y;
    painter.galley(pos2(rect.min.x + 20.0, status_y), galley, c(color));

    let list = Rect::from_min_max(pos2(rect.min.x + 12.0, status_y + status_h + 12.0), pos2(rect.max.x - 12.0, rect.max.y - 12.0));
    let mut list_ui = ui.new_child(egui::UiBuilder::new().max_rect(list).layout(egui::Layout::top_down(egui::Align::Min)));
    let mut clicked = None;
    egui::ScrollArea::vertical().id_salt("index-results").auto_shrink([false, false]).show(&mut list_ui, |ui| {
        for (i, found) in state.results.iter().enumerate() {
            let (row, resp) = ui.allocate_exact_size(vec2(list.width() - 10.0, ROW_H), Sense::click());
            let selected = state.selected == Some(i);
            result_row(ui.painter(), core, &state.pictures, found, row.shrink2(vec2(8.0, 4.0)), selected, resp.hovered());
            if resp.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
            if resp.clicked() {
                clicked = Some(i);
            }
        }
        if state.results.is_empty() && state.pending.is_none() && state.error.is_none() {
            ui.add_space(24.0);
            let text = match state.searched.as_deref() {
                Some(q) if !q.is_empty() => format!("Nothing in the index matches \"{q}\". Try a company, a person or a topic."),
                _ => "The index has no agents yet.".to_string(),
            };
            ui.label(egui::RichText::new(text).font(theme::ui(14.5)).color(c(theme::TEXT_2)));
        }
    });
    if let Some(i) = clicked
        && state.selected != Some(i)
    {
        state.selected = Some(i);
        state.anim = PreviewAnim::Working;
        state.anim_since = core.world.time;
        state.build_selected();
    }
}

fn result_row(painter: &Painter, core: &Core, pictures: &Pictures, found: &Found, row: Rect, selected: bool, hovered: bool) {
    let m = &found.entry.manifest;
    let (fill, stroke) = if selected {
        (c([0x1E, 0x19, 0x10]), Stroke::new(1.0, c([0x5A, 0x44, 0x24])))
    } else if hovered {
        (c([0x1A, 0x1F, 0x2A]), Stroke::NONE)
    } else {
        (c([0x16, 0x1A, 0x23]), Stroke::NONE)
    };
    painter.rect(row, CornerRadius::same(14), fill, stroke, StrokeKind::Inside);
    let pic = Rect::from_min_size(row.min + vec2(12.0, 12.0), vec2(row.height() - 24.0, row.height() - 24.0));
    picture(painter, pictures, m.picture.as_deref(), &m.name, pic);

    let x = pic.max.x + 16.0;
    let right = row.max.x - 14.0;
    let name = painter.text(pos2(x, row.min.y + 12.0), Align2::LEFT_TOP, &m.name, theme::ui_semibold(16.5), c(theme::TEXT));
    let kind = if m.kind == ManifestKind::Person { "PERSON" } else { "ORGANISATION" };
    painter.text(pos2(right, row.min.y + 16.0), Align2::RIGHT_TOP, kind, theme::mono(9.5), c(theme::TEXT_2));
    if imported_as(core, &m.id).is_some() {
        small_pill(painter, pos2(name.max.x + 10.0, name.center().y), "IN YOUR SPACE", MINT);
    }
    let owner_y = row.min.y + 38.0;
    let owner = if m.kind == ManifestKind::Person {
        format!("{} · @{}", m.owner.name, m.owner.domain)
    } else {
        format!("{} · {}", m.owner.name, m.owner.domain)
    };
    let owner = fit(painter, &owner, theme::ui(13.0), right - x - 120.0);
    let owner_r = painter.text(pos2(x, owner_y), Align2::LEFT_TOP, owner, theme::ui(13.0), c([0xC9, 0xCE, 0xD9]));
    verified_mark(painter, pos2(owner_r.max.x + 10.0, owner_r.center().y), found.entry.verified, m.kind);

    let welcome = fit(painter, &m.welcome, theme::ui(13.5), right - x);
    painter.text(pos2(x, row.min.y + 60.0), Align2::LEFT_TOP, welcome, theme::ui(13.5), c(theme::TEXT_2));
    let mut cx = x;
    for term in &m.search_terms {
        let w = painter.layout_no_wrap(term.clone(), theme::mono(10.5), c(theme::TEXT)).size().x + 14.0;
        if cx + w > right {
            break;
        }
        let chip = Rect::from_min_size(pos2(cx, row.min.y + 84.0), vec2(w, 18.0));
        painter.rect(chip, CornerRadius::same(6), c([0x1C, 0x21, 0x2C]), Stroke::NONE, StrokeKind::Inside);
        painter.text(chip.center(), Align2::CENTER_CENTER, term, theme::mono(10.5), c([0xB9, 0xBF, 0xCC]));
        cx = chip.max.x + 6.0;
    }
}

/// The picture of a result, or its initial on a coloured tile while it loads or failed.
pub(crate) fn picture(painter: &Painter, pictures: &Pictures, url: Option<&str>, name: &str, rect: Rect) {
    let radius = CornerRadius::same(12);
    if let Some(tex) = url.and_then(|u| pictures.texture(u)) {
        let shape = egui::epaint::RectShape::filled(rect, radius, Color32::WHITE)
            .with_texture(tex, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)));
        painter.add(shape);
    } else {
        painter.rect_filled(rect, radius, c([0x1C, 0x21, 0x2C]));
        let initial: String = name.chars().next().map(|c| c.to_uppercase().collect()).unwrap_or_default();
        painter.text(rect.center(), Align2::CENTER_CENTER, initial, theme::ui_bold(rect.height() * 0.4), c(theme::TEXT_2));
    }
}

/// "VERIFIED" with a check for organisations whose domain proved the
/// manifest, "UNVERIFIED" otherwise. People are verified later.
pub(crate) fn verified_mark(painter: &Painter, left_center: Pos2, verified: bool, kind: ManifestKind) {
    if verified {
        shield(painter, left_center + vec2(6.0, 0.0), c(MINT));
        painter.text(left_center + vec2(16.0, 0.0), Align2::LEFT_CENTER, "VERIFIED", theme::mono(10.0), c(MINT));
    } else {
        let text = if kind == ManifestKind::Person { "NOT VERIFIED YET" } else { "UNVERIFIED" };
        painter.text(left_center, Align2::LEFT_CENTER, text, theme::mono(10.0), c([0x8A, 0x91, 0xA0]));
    }
}

/// A small shield with a check, as in screen 4.
fn shield(painter: &Painter, center: Pos2, color: Color32) {
    let s = 5.5;
    let points = vec![
        center + vec2(0.0, -s),
        center + vec2(s * 0.9, -s * 0.6),
        center + vec2(s * 0.75, s * 0.35),
        center + vec2(0.0, s),
        center + vec2(-s * 0.75, s * 0.35),
        center + vec2(-s * 0.9, -s * 0.6),
    ];
    painter.add(egui::Shape::closed_line(points, Stroke::new(1.2, color)));
    painter.line_segment([center + vec2(-2.2, 0.0), center + vec2(-0.5, 1.8)], Stroke::new(1.2, color));
    painter.line_segment([center + vec2(-0.5, 1.8), center + vec2(2.5, -1.8)], Stroke::new(1.2, color));
}

pub(crate) fn small_pill(painter: &Painter, left_center: Pos2, text: &str, color: [u8; 3]) -> Rect {
    let galley = painter.layout_no_wrap(text.to_string(), theme::mono(9.5), c(color));
    let size = galley.size() + vec2(12.0, 5.0);
    let rect = Rect::from_min_size(pos2(left_center.x, left_center.y - size.y / 2.0), size);
    painter.rect(rect, CornerRadius::same(5), ca(color, 24), Stroke::new(1.0, ca(color, 80)), StrokeKind::Inside);
    painter.galley(rect.min + vec2(6.0, 2.5), galley, c(color));
    rect
}

fn detail_pane(ui: &mut Ui, core: &mut Core, state: &mut IndexState, rect: Rect) {
    let painter = ui.painter();
    super::panel(painter, rect, 18);
    let Some(found) = state.selected() else {
        let text = if state.pending.is_some() { "Searching the index" } else { "Pick an agent on the left to see it here" };
        painter.text(rect.center(), Align2::CENTER_CENTER, text, theme::ui(16.0), c(theme::TEXT_2));
        return;
    };
    let m = found.entry.manifest.clone();
    let entry = found.entry.clone();
    let verified = found.entry.verified;
    let figure = found.figure.as_ref().map(|f| f.as_ref().map(|d| (d.triangles, d.budget)).map_err(Clone::clone));

    // Who it is.
    let title = painter.text(rect.min + vec2(20.0, 18.0), Align2::LEFT_TOP, &m.name, theme::ui_bold(22.0), c(theme::TEXT));
    let owner = if m.kind == ManifestKind::Person {
        format!("{} · @{}", m.owner.name, m.owner.domain)
    } else {
        format!("{} · {}", m.owner.name, m.owner.domain)
    };
    let owner_r =
        painter.text(pos2(title.max.x + 12.0, title.center().y + 2.0), Align2::LEFT_CENTER, owner, theme::ui(14.0), c(theme::TEXT_2));
    verified_mark(painter, pos2(owner_r.max.x + 12.0, owner_r.center().y), verified, m.kind);
    if let Some(Ok((triangles, _))) = &figure {
        painter.text(
            pos2(rect.max.x - 20.0, title.center().y),
            Align2::RIGHT_CENTER,
            format!("{triangles} / {EXTERNAL_TRIANGLES} TRIANGLES"),
            theme::mono(11.0),
            c(theme::TEXT_2),
        );
    }

    // The live figure. A person's profile below it needs more room (D26).
    let bottom = if entry.profile.is_some() { 330.0 } else { 272.0 };
    let stage = Rect::from_min_max(pos2(rect.min.x + 20.0, rect.min.y + 60.0), pos2(rect.max.x - 20.0, rect.max.y - bottom));
    match &figure {
        Some(Ok(_)) => {
            let ppp = ui.ctx().pixels_per_point();
            state.preview_px = ((stage.width() * ppp) as u32, (stage.height() * ppp) as u32);
            let resp = ui.interact(stage, Id::new("index-stage"), Sense::drag());
            if resp.dragged() {
                state.yaw += resp.drag_delta().x * 0.012;
                ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
            } else if resp.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
            }
            if let Some(tex) = core.preview_texture() {
                ui.painter().image(tex, stage, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
            }
            ui.painter().text(
                pos2(stage.center().x, stage.max.y - 14.0),
                Align2::CENTER_CENTER,
                "DRAG TO TURN",
                theme::mono(11.0),
                c([0x6B, 0x72, 0x82]),
            );
        }
        Some(Err(e)) => {
            let painter = ui.painter();
            painter.rect(stage, CornerRadius::same(14), c(theme::TERMINAL_BG), Stroke::NONE, StrokeKind::Inside);
            let galley = painter.layout(format!("No preview. {e}"), theme::ui(14.0), c(theme::TEXT_2), stage.width() - 80.0);
            painter.galley(stage.center() - galley.size() / 2.0, galley, c(theme::TEXT_2));
        }
        None => {}
    }

    // Animations.
    let y = stage.max.y + 16.0;
    label(ui.painter(), pos2(rect.min.x + 20.0, y), "ANIMATIONS");
    let anims = [
        (PreviewAnim::Working, "Working", theme::WORKING),
        (PreviewAnim::NeedsYou, "Needs you", theme::NEEDS_YOU),
        (PreviewAnim::Resting, "Resting", theme::RESTING),
        (PreviewAnim::Walk, "Walk", [0xC9, 0xCE, 0xD9]),
        (PreviewAnim::Hop, "Hop", [0xC9, 0xCE, 0xD9]),
    ];
    let mut x = rect.min.x + 20.0;
    for (anim, text, dot) in anims {
        let w = ui.painter().layout_no_wrap(text.to_string(), theme::ui(13.5), c(theme::TEXT)).size().x + 38.0;
        let r = Rect::from_min_size(pos2(x, y + 18.0), vec2(w, 36.0));
        let on = state.anim == anim;
        let resp = ui.interact(r, Id::new(("index-anim", text)), Sense::click());
        let painter = ui.painter();
        painter.rect(
            r,
            CornerRadius::same(11),
            if on { c([0x1E, 0x19, 0x10]) } else { c([0x16, 0x1A, 0x23]) },
            Stroke::new(1.0, if on { c([0xC7, 0x96, 0x4A]) } else { c([0x26, 0x2C, 0x38]) }),
            StrokeKind::Inside,
        );
        painter.circle_filled(pos2(r.min.x + 15.0, r.center().y), 4.0, c(dot));
        painter.text(
            pos2(r.min.x + 26.0, r.center().y),
            Align2::LEFT_CENTER,
            text,
            theme::ui(13.5),
            if on { c([0xF3, 0xE2, 0xC6]) } else { c([0xC9, 0xCE, 0xD9]) },
        );
        if resp.clicked() {
            state.anim = anim;
            state.anim_since = core.world.time;
        }
        x = r.max.x + 8.0;
    }

    // A person shows photos and profile instead of the welcome text (D26).
    if let Some(profile) = &entry.profile {
        let area = Rect::from_min_max(pos2(rect.min.x + 20.0, y + 72.0), pos2(rect.max.x - 20.0, rect.max.y - 16.0));
        super::feed::person_details(ui, core, &state.pictures, &entry, profile, area);
        return;
    }

    // The welcome text as the agent would say it.
    let painter = ui.painter();
    let inner = rect.width() - 40.0;
    let mut y = y + 72.0;
    label(painter, pos2(rect.min.x + 20.0, y), "WELCOME");
    y += 20.0;
    let galley = painter.layout(m.welcome.clone(), theme::ui(14.5), c(theme::TEXT), inner - 26.0);
    let bubble = Rect::from_min_size(pos2(rect.min.x + 20.0, y), vec2(galley.size().x + 26.0, galley.size().y + 18.0));
    painter.rect(bubble, CornerRadius::same(12), c([0x1A, 0x17, 0x10]), Stroke::new(1.0, c([0x3A, 0x2E, 0x18])), StrokeKind::Inside);
    painter.galley(bubble.min + vec2(13.0, 9.0), galley, c(theme::TEXT));
    y = bubble.max.y + 12.0;

    // What leaves the machine.
    let host = m.endpoint.url.split("://").nth(1).unwrap_or(&m.endpoint.url).split('/').next().unwrap_or("").to_string();
    let privacy = m.privacy.clone().unwrap_or_else(|| format!("{} sees what you say to {}.", m.owner.name, m.name));
    let note = format!("{privacy} Only what you type goes to {host}. {} gets no files, no tools and no access to your apps.", m.name);
    y = privacy_box(painter, Rect::from_min_size(pos2(rect.min.x + 20.0, y), vec2(inner, 0.0)), &note).max.y + 14.0;

    // Facts. URLs keep their case, paths are case sensitive.
    let x = label(painter, pos2(rect.min.x + 20.0, y), "A2A 1.0 ENDPOINT").max.x + 10.0;
    let mut url_end = rect.max.x - 20.0;
    if !m.languages.is_empty() {
        let langs = format!("SPEAKS {}", m.languages.join(" ").to_uppercase());
        url_end = painter.text(pos2(url_end, y), Align2::RIGHT_TOP, langs, theme::mono(11.0), c(theme::TEXT_2)).min.x - 16.0;
    }
    let url = fit(painter, &m.endpoint.url, theme::mono(11.0), url_end - x);
    painter.text(pos2(x, y), Align2::LEFT_TOP, url, theme::mono(11.0), c([0xC9, 0xCE, 0xD9]));
}

/// A teal box with a lock and the text, as the privacy notice of screen 4.
/// `rect` gives the position and width, the height follows the text.
pub fn privacy_box(painter: &Painter, rect: Rect, text: &str) -> Rect {
    let galley = painter.layout(text.to_string(), theme::ui(13.0), c([0xD8, 0xEE, 0xEA]), rect.width() - 52.0);
    let rect = Rect::from_min_size(rect.min, vec2(rect.width(), galley.size().y + 20.0));
    painter.rect(rect, CornerRadius::same(12), ca(TEAL, 46), Stroke::new(1.0, ca(MINT, 70)), StrokeKind::Inside);
    lock(painter, pos2(rect.min.x + 20.0, rect.min.y + 10.0 + 8.0), c(MINT));
    painter.galley(rect.min + vec2(40.0, 10.0), galley, c([0xD8, 0xEE, 0xEA]));
    rect
}

fn lock(painter: &Painter, center: Pos2, color: Color32) {
    let body = Rect::from_center_size(center + vec2(0.0, 2.5), vec2(11.0, 8.0));
    painter.rect_stroke(body, CornerRadius::same(2), Stroke::new(1.3, color), StrokeKind::Inside);
    let arc: Vec<Pos2> = (0..=12)
        .map(|k| std::f32::consts::PI * k as f32 / 12.0)
        .map(|t| pos2(center.x + 3.4 * t.cos(), center.y - 1.5 - 3.6 * t.sin()))
        .collect();
    painter.add(egui::Shape::line(arc, Stroke::new(1.3, color)));
}

/// The "Get an external agent" row under "+ New agent" in the sidebar.
/// Returns true when clicked.
pub fn sidebar_row(ui: &mut Ui, row: Rect) -> bool {
    let r = ui.interact(row, Id::new("get-external"), Sense::click());
    let painter = ui.painter();
    let fill = if r.hovered() { ca(TEAL, 40) } else { ca(TEAL, 18) };
    painter.rect(row, CornerRadius::same(10), fill, Stroke::new(1.0, ca(MINT, if r.hovered() { 110 } else { 60 })), StrokeKind::Inside);
    let text = "Get an external agent";
    let w = painter.layout_no_wrap(text.to_string(), theme::ui(14.0), c(MINT)).size().x;
    let door = Rect::from_center_size(pos2(row.center().x - w / 2.0 - 4.0, row.center().y), vec2(10.0, 14.0));
    painter.rect_stroke(door, CornerRadius { nw: 5, ne: 5, sw: 0, se: 0 }, Stroke::new(1.3, c(MINT)), StrokeKind::Inside);
    painter.circle_filled(pos2(door.max.x - 3.0, door.center().y + 1.0), 1.1, c(MINT));
    painter.text(pos2(row.center().x + 8.0, row.center().y), Align2::CENTER_CENTER, text, theme::ui(14.0), c([0xCF, 0xF3, 0xEC]));
    if r.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    r.clicked()
}

/// The owner domain and the verified flag of an imported agent.
pub fn external(core: &Core, id: &str) -> Option<(String, bool)> {
    core.agent_dir(id).and_then(|d| d.config.external.as_ref()).map(|e| (e.domain.clone(), e.verified))
}

/// "EXTERNAL" right of an agent's name in the sidebar.
pub fn sidebar_badge(painter: &Painter, core: &Core, id: &str, name: Rect, max_x: f32) {
    if external(core, id).is_some() && name.max.x + 80.0 < max_x {
        small_pill(painter, pos2(name.max.x + 8.0, name.center().y), "EXTERNAL", MINT);
    }
}

/// The sidebar's second line for an imported agent starts with its owner's domain.
pub fn sidebar_sub(core: &Core, id: &str, sub: String) -> String {
    match external(core, id) {
        Some((domain, _)) if sub.is_empty() => domain,
        Some((domain, _)) => format!("{domain} · {sub}"),
        None => sub,
    }
}

/// A badge with the domain above an imported agent's name tag in the space.
pub fn tag_badge(painter: &Painter, core: &Core, id: &str, tag: Rect) {
    let Some((domain, verified)) = external(core, id) else { return };
    let text = format!("EXTERNAL · {domain}");
    let galley = painter.layout_no_wrap(text, theme::mono(9.5), c(MINT));
    let w = galley.size().x + 14.0 + if verified { 14.0 } else { 0.0 };
    let rect = Rect::from_min_size(pos2(tag.center().x - w / 2.0, tag.min.y - 22.0), vec2(w, 18.0));
    painter.rect(rect, CornerRadius::same(6), ca([0x0F, 0x2A, 0x28], 236), Stroke::new(1.0, ca(MINT, 90)), StrokeKind::Inside);
    let mut x = rect.min.x + 7.0;
    if verified {
        shield(painter, pos2(x + 5.0, rect.center().y), c(MINT));
        x += 14.0;
    }
    painter.galley(pos2(x, rect.center().y - galley.size().y / 2.0), galley, c(MINT));
}

/// "EXTERNAL · domain" in the agent view header. Returns its rect.
/// Falls back to "EXTERNAL" alone, or nothing, when it would pass `max_x`.
pub fn header_badge(painter: &Painter, core: &Core, id: &str, left_center: Pos2, max_x: f32) -> Option<Rect> {
    let (domain, verified) = external(core, id)?;
    let full = format!("EXTERNAL · {domain}{}", if verified { " · VERIFIED" } else { "" });
    let text = [full, "EXTERNAL".to_string()]
        .into_iter()
        .find(|t| left_center.x + painter.layout_no_wrap(t.clone(), theme::mono(9.5), Color32::WHITE).size().x + 12.0 <= max_x)?;
    Some(small_pill(painter, left_center, &text, MINT))
}

/// Height of the privacy note an imported agent shows above the chat input, 0 for others.
pub fn privacy_height(core: &Core, id: &str, painter: &Painter, width: f32) -> f32 {
    match privacy_text(core, id) {
        Some(text) => painter.layout(text, theme::ui(13.0), Color32::WHITE, width - 52.0).size().y + 20.0 + 10.0,
        None => 0.0,
    }
}

/// Draws the privacy note of an imported agent with its top at `top`.
pub fn privacy_note(painter: &Painter, core: &Core, id: &str, left: f32, top: f32, width: f32) {
    if let Some(text) = privacy_text(core, id) {
        privacy_box(painter, Rect::from_min_size(pos2(left, top), vec2(width, 0.0)), &text);
    }
}

fn privacy_text(core: &Core, id: &str) -> Option<String> {
    let dir: &AgentDir = core.agent_dir(id)?;
    let external = dir.config.external.as_ref()?;
    let host =
        dir.config.runtime.as_ref().and_then(|r| r.url.as_deref()).and_then(|u| u.split("://").nth(1)).and_then(|r| r.split('/').next());
    let privacy = external.privacy.clone().unwrap_or_else(|| format!("{} sees what you say to {}.", external.domain, dir.config.name));
    Some(match host {
        Some(host) => format!("{privacy} Messages go to {host}. No files, no tools."),
        None => privacy,
    })
}

/// Pictures of search results, fetched on threads and kept as textures.
/// The feed and the profile use it too, also for local files (absolute paths).
pub(crate) struct Pictures {
    items: HashMap<String, Picture>,
    tx: Sender<(String, Result<egui::ColorImage, String>)>,
    rx: Receiver<(String, Result<egui::ColorImage, String>)>,
    /// For a repaint when a picture arrives.
    ctx: Arc<Mutex<Option<egui::Context>>>,
}

enum Picture {
    Loading,
    Decoded(egui::ColorImage),
    Ready(egui::TextureHandle),
    Failed,
}

impl Pictures {
    pub(crate) fn new() -> Self {
        let (tx, rx) = channel();
        Self { items: HashMap::new(), tx, rx, ctx: Arc::new(Mutex::new(None)) }
    }

    pub(crate) fn request(&mut self, url: &str) {
        if self.items.contains_key(url) {
            return;
        }
        if !(url.starts_with("https://") || url.starts_with("http://") || url.starts_with('/')) {
            self.items.insert(url.to_string(), Picture::Failed);
            return;
        }
        self.items.insert(url.to_string(), Picture::Loading);
        let key = url.to_string();
        let (url, tx, ctx) = (url.to_string(), self.tx.clone(), self.ctx.clone());
        let spawned = std::thread::Builder::new().name("index-picture".into()).spawn(move || {
            let result = fetch_picture(&url);
            if let Err(e) = &result {
                tracing::info!("picture {url} not shown ({e})");
            }
            let _ = tx.send((url, result));
            if let Some(ctx) = ctx.lock().ok().and_then(|c| c.clone()) {
                ctx.request_repaint();
            }
        });
        if spawned.is_err() {
            self.items.insert(key, Picture::Failed);
        }
    }

    fn receive(&mut self, url: String, result: Result<egui::ColorImage, String>) {
        self.items.insert(url, result.map_or(Picture::Failed, Picture::Decoded));
    }

    /// Turns arrived pictures into textures.
    pub(crate) fn poll(&mut self, ctx: &egui::Context) {
        if let Ok(mut slot) = self.ctx.lock()
            && slot.is_none()
        {
            *slot = Some(ctx.clone());
        }
        while let Ok((url, result)) = self.rx.try_recv() {
            self.receive(url, result);
        }
        for (url, item) in self.items.iter_mut() {
            if matches!(item, Picture::Decoded(_))
                && let Picture::Decoded(image) = std::mem::replace(item, Picture::Failed)
            {
                *item = Picture::Ready(ctx.load_texture(url.clone(), image, egui::TextureOptions::LINEAR));
            }
        }
    }

    pub(crate) fn wait(&mut self, timeout: Duration) {
        let until = Instant::now() + timeout;
        while self.items.values().any(|p| matches!(p, Picture::Loading)) {
            let Some(left) = until.checked_duration_since(Instant::now()) else { return };
            match self.rx.recv_timeout(left) {
                Ok((url, result)) => self.receive(url, result),
                Err(_) => return,
            }
        }
    }

    pub(crate) fn texture(&self, url: &str) -> Option<egui::TextureId> {
        match self.items.get(url) {
            Some(Picture::Ready(handle)) => Some(handle.id()),
            _ => None,
        }
    }
}

/// Fetches and decodes a picture, at most 2 MB and scaled to 256 pixels.
/// An absolute path is read from disk, for the user's own photos (D26).
fn fetch_picture(url: &str) -> Result<egui::ColorImage, String> {
    let bytes = if url.starts_with('/') {
        let path = std::path::Path::new(url);
        if std::fs::metadata(path).map_err(|e| e.to_string())?.len() > 2 * 1024 * 1024 {
            return Err("larger than 2 MB".into());
        }
        std::fs::read(path).map_err(|e| e.to_string())?
    } else {
        let config = ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(10))).build();
        let agent = ureq::Agent::new_with_config(config);
        let mut response = agent.get(url).call().map_err(|e| e.to_string())?;
        response.body_mut().with_config().limit(2 * 1024 * 1024).read_to_vec().map_err(|e| e.to_string())?
    };
    let image = image::load_from_memory(&bytes).map_err(|e| e.to_string())?;
    let image = if image.width() > 256 || image.height() > 256 { image.thumbnail(256, 256) } else { image }.to_rgba8();
    Ok(egui::ColorImage::from_rgba_unmultiplied([image.width() as usize, image.height() as usize], image.as_raw()))
}
