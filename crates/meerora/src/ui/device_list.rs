//! The devices of the space in the sidebar (screen 1, D7, D8) and "Ask an
//! agent to build a device".
//!
//! The list has one row per device in the space: its glyph, its name, the
//! line its status binding shows and a padlock for its capabilities. A
//! device an agent made also shows its maker. A click opens the device card
//! (ui/devices.rs), next to the device or, when its wall is cut away, next
//! to the row.
//!
//! The button at the end of the list opens a small dialog. The user picks an
//! agent, describes the device in one line and presses Ask. meerora then
//! sends the agent a chat message (`Command::Say`) with the request and where
//! the device API is, and opens the agent's view so the user can follow.
//! Mock agents only play the demo scenario and external agents have no files
//! here (D25), so neither can build a device. The dialog greys them out.

use egui::{Align2, Color32, CornerRadius, Id, Order, Painter, Pos2, Rect, Sense, Shape, Stroke, StrokeKind, Ui, pos2, vec2};
use meerora_protocol::{Command, RuntimeKind};

use super::devices::{Glyph, draw_glyph, glyph_for};
use super::space::{HEADER_H, sidebar_right};
use super::{AppTab, ButtonStyle, View, button, fit, label, open_agent, panel, pill, portrait};
use crate::engine::Core;
use crate::theme::{self, c, ca};

pub const ROW_H: f32 = 44.0;
pub const BUTTON_H: f32 = 44.0;
const DIALOG_W: f32 = 452.0;
/// What the description field suggests, and the examples under it.
pub const EXAMPLES: [&str; 2] = ["a lamp that glows while CI runs", "a plant that wilts when the disk is full"];

/// The padlock of a row (D8).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Lock {
    None,
    /// Its actions need capabilities the user has not allowed yet. It asks at the first click.
    Asks,
    /// It holds this many capabilities.
    Held(usize),
    /// It waits for the user's answer to a capability request.
    Pending,
}

/// One row of the devices list.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub id: String,
    pub name: String,
    pub glyph: Glyph,
    /// What its status binding shows now, for an agent's device after its maker.
    pub state: String,
    /// The agent that made it, id and name. `None` for the space's own devices.
    pub owner: Option<(String, String)>,
    pub lock: Lock,
}

/// The rows of the devices in the space: the space's own devices by name,
/// then the agents' devices by maker and name.
pub fn rows(core: &Core) -> Vec<Row> {
    let mut rows: Vec<Row> = core
        .devices
        .items
        .iter()
        .filter(|d| core.devices.shown(d))
        .map(|d| {
            let info = core.devices.info(&d.id);
            let granted = info.map_or(0, |i| i.granted.len());
            let pending = core.ui.approvals.devices.iter().any(|r| r.device == d.id);
            let owner = d.owner.as_ref().map(|id| (id.clone(), core.world.agent(id).map_or_else(|| id.clone(), |a| a.name.clone())));
            let status = d.desc.status_text(&d.params).unwrap_or_default();
            let lock = if pending {
                Lock::Pending
            } else if granted > 0 {
                Lock::Held(granted)
            } else if owner.is_some() {
                // An agent's device loads only when its maker holds what it names (D8).
                let needs = info.map_or_else(|| d.desc.capabilities().len(), |i| i.needs.len());
                if needs > 0 { Lock::Held(needs) } else { Lock::None }
            } else if d.desc.actions.iter().any(|a| a.capability().is_some()) {
                Lock::Asks
            } else {
                Lock::None
            };
            let state = match &owner {
                Some((_, name)) if status.is_empty() => format!("by {name}"),
                Some((_, name)) => format!("by {name} · {status}"),
                None => status,
            };
            Row { id: d.id.clone(), name: d.desc.name.clone(), glyph: glyph_for(d), state, owner, lock }
        })
        .collect();
    rows.sort_by(|a, b| (a.owner.as_ref().map(|o| &o.1), &a.name, &a.id).cmp(&(b.owner.as_ref().map(|o| &o.1), &b.name, &b.id)));
    rows
}

/// Height of the devices section: the label, the rows (or a line when there
/// are none) and the button. Collapsed, only the label.
pub fn height(rows: usize, collapsed: bool) -> f32 {
    if collapsed { 22.0 } else { 22.0 + rows.max(1) as f32 * ROW_H + 10.0 + BUTTON_H }
}

/// Whether the devices section is collapsed, kept in `sidebar.json` of the
/// data directory so it stays that way after a restart.
pub fn collapsed(core: &Core) -> bool {
    *core.ui.devices_collapsed.get_or_init(|| {
        std::fs::read_to_string(core.assets.data_dir.join(SIDEBAR_FILE))
            .ok()
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
            .and_then(|v| v.get("devices_collapsed").and_then(|b| b.as_bool()))
            .unwrap_or(false)
    })
}

fn set_collapsed(core: &mut Core, value: bool) {
    core.ui.devices_collapsed = std::sync::OnceLock::from(value);
    let path = core.assets.data_dir.join(SIDEBAR_FILE);
    if let Err(e) = std::fs::write(&path, serde_json::json!({ "devices_collapsed": value }).to_string()) {
        tracing::warn!("cannot save {} ({e})", path.display());
    }
}

const SIDEBAR_FILE: &str = "sidebar.json";

/// What a click in the devices section asked for.
pub enum Click {
    Device(String),
    Ask,
}

/// Draws the devices section from `top`, `width` wide, inside the sidebar.
/// `bottom` is where the sidebar's content ends, the button keeps to it when
/// the list is short, like the mockup. Marks the hovered device so the scene
/// lights it.
pub fn draw(ui: &mut Ui, core: &mut Core, rows: &[Row], top: Pos2, width: f32, bottom: f32) -> Option<Click> {
    let mut click = None;
    let mut hovered: Option<String> = None;
    let x = top.x;
    let mut y = top.y;
    // The label folds the section: a chevron and the number of devices.
    let folded = collapsed(core);
    let head = Rect::from_min_size(pos2(x - 4.0, y - 4.0), vec2(width + 8.0, 20.0));
    let toggle = ui.interact(head, Id::new("devices-fold"), Sense::click());
    if toggle.hovered() {
        ui.painter().rect_filled(head, CornerRadius::same(6), c([0x16, 0x1A, 0x23]));
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let painter = ui.painter();
    label(painter, pos2(x, y), "DEVICES");
    let count = format!("{}", rows.len());
    painter.text(pos2(x + 62.0, y + 6.0), Align2::LEFT_CENTER, count, theme::mono(10.5), c(theme::TEXT_2));
    let chevron = pos2(head.max.x - 14.0, y + 6.0);
    let stroke = Stroke::new(1.5, c(theme::TEXT_2));
    if folded {
        painter.line_segment([chevron + vec2(-2.0, -4.0), chevron + vec2(2.0, 0.0)], stroke);
        painter.line_segment([chevron + vec2(2.0, 0.0), chevron + vec2(-2.0, 4.0)], stroke);
    } else {
        painter.line_segment([chevron + vec2(-4.0, -2.0), chevron + vec2(0.0, 2.0)], stroke);
        painter.line_segment([chevron + vec2(0.0, 2.0), chevron + vec2(4.0, -2.0)], stroke);
    }
    let toggle = toggle.on_hover_text(if folded { "Show the devices" } else { "Hide the devices" });
    if toggle.clicked() {
        set_collapsed(core, !folded);
    }
    if folded {
        return None;
    }
    y += 22.0;
    if rows.is_empty() {
        let painter = ui.painter();
        painter.text(pos2(x + 2.0, y + ROW_H / 2.0), Align2::LEFT_CENTER, "No devices in this room yet", theme::ui(13.0), c(theme::TEXT_2));
        y += ROW_H;
    }
    for row in rows {
        let rect = Rect::from_min_size(pos2(x, y), vec2(width, ROW_H));
        let r = ui.interact(rect, Id::new(("device-row", &row.id)), Sense::click());
        let open = core.devices.card.as_deref() == Some(row.id.as_str());
        let lit = r.hovered() || core.devices.device(&row.id).is_some_and(|d| d.hover);
        draw_row(ui.painter(), core, row, rect, open, lit);
        if r.hovered() {
            hovered = Some(row.id.clone());
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        if r.clicked() {
            // The card shows next to the row when the device is out of sight.
            ui.ctx().data_mut(|d| d.insert_temp(row_anchor_key(), (row.id.clone(), rect.center().y)));
            click = Some(Click::Device(row.id.clone()));
        }
        y += ROW_H;
    }
    let button_top = (y + 10.0).max(bottom - BUTTON_H);
    let rect = Rect::from_min_size(pos2(x, button_top), vec2(width, BUTTON_H));
    if ask_button(ui, rect) {
        click = Some(Click::Ask);
    }
    ui.ctx().data_mut(|d| d.insert_temp(Id::new("ask-device-button"), rect));
    if let Some(id) = hovered
        && let Some(d) = core.devices.items.iter_mut().find(|d| d.id == id)
    {
        d.hover = true;
    }
    click
}

/// Where `ui/devices.rs` puts a card whose device is out of sight: the
/// device's id and the middle of its row.
pub fn row_anchor_key() -> Id {
    Id::new("device-row-anchor")
}

/// The card of `id` next to its row in the list, if the row opened it.
pub fn row_anchor(ctx: &egui::Context, id: &str) -> Option<Pos2> {
    let (device, y) = ctx.data(|d| d.get_temp::<(String, f32)>(row_anchor_key()))?;
    (device == id).then(|| pos2(sidebar_right(), y))
}

fn draw_row(painter: &Painter, core: &Core, row: &Row, rect: Rect, open: bool, lit: bool) {
    let fill = match (open, lit) {
        (true, _) => c([0x1D, 0x22, 0x30]),
        (false, true) => c([0x18, 0x1C, 0x25]),
        (false, false) => Color32::TRANSPARENT,
    };
    painter.rect_filled(rect, CornerRadius::same(10), fill);
    let glyph_at = pos2(rect.min.x + 18.0, rect.center().y);
    draw_glyph(painter, row.glyph, glyph_at, 0.9);
    // The maker's portrait sits on the glyph's corner.
    if let Some(agent) = row.owner.as_ref().and_then(|(id, _)| core.world.agent(id)) {
        let at = glyph_at + vec2(9.0, 8.0);
        painter.circle_filled(at, 9.5, c(theme::PANEL));
        portrait(painter, Rect::from_center_size(at, vec2(18.0, 18.0)), core, agent);
    }
    let right = rect.max.x - 10.0;
    let lock_left = draw_lock(painter, &row.lock, pos2(right, rect.center().y));
    let text_x = rect.min.x + 40.0;
    let text_w = lock_left - text_x - 8.0;
    let name = fit(painter, &row.name, theme::ui(14.0), text_w);
    painter.text(pos2(text_x, rect.min.y + 6.0), Align2::LEFT_TOP, name, theme::ui(14.0), c(theme::TEXT));
    if !row.state.is_empty() {
        let state = fit(painter, &row.state, theme::ui(12.0), text_w);
        painter.text(pos2(text_x, rect.min.y + 25.0), Align2::LEFT_TOP, state, theme::ui(12.0), c(theme::TEXT_2));
    }
}

/// The padlock right of a row, its right edge at `right`. Returns its left edge.
fn draw_lock(painter: &Painter, lock: &Lock, right: Pos2) -> f32 {
    match lock {
        Lock::None => right.x,
        Lock::Pending => pill(painter, right, "NEEDS OK", theme::NEEDS_YOU).min.x,
        Lock::Asks => {
            super::approvals::lock(painter, pos2(right.x - 8.0, right.y), 12.0, ca(theme::TEXT_2, 170));
            right.x - 16.0
        }
        Lock::Held(n) => {
            let w = if *n > 1 { 34.0 } else { 24.0 };
            let chip = Rect::from_min_size(pos2(right.x - w, right.y - 11.0), vec2(w, 22.0));
            painter.rect(
                chip,
                CornerRadius::same(7),
                ca([0x16, 0x14, 0x28], 235),
                Stroke::new(1.0, ca(theme::SYSTEM, 150)),
                StrokeKind::Inside,
            );
            let at = if *n > 1 { pos2(chip.min.x + 11.0, chip.center().y) } else { chip.center() };
            super::approvals::lock(painter, at, 11.0, c(theme::SYSTEM_2));
            if *n > 1 {
                painter.text(
                    pos2(chip.max.x - 8.0, chip.center().y),
                    Align2::CENTER_CENTER,
                    n.to_string(),
                    theme::mono(10.0),
                    c(theme::SYSTEM_2),
                );
            }
            chip.min.x
        }
    }
}

/// "+ Ask an agent to build a device" in a dashed frame, as in screen 1.
fn ask_button(ui: &mut Ui, rect: Rect) -> bool {
    let r = ui.interact(rect, Id::new("ask-device"), Sense::click());
    let painter = ui.painter();
    if r.hovered() {
        painter.rect_filled(rect, CornerRadius::same(12), c([0x18, 0x1C, 0x25]));
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let color = if r.hovered() { c([0x4A, 0x52, 0x63]) } else { c([0x34, 0x3B, 0x4A]) };
    painter.extend(Shape::dashed_line(&rounded_outline(rect.shrink(0.5), 12.0), Stroke::new(1.0, color), 4.0, 3.0));
    let text = "Ask an agent to build a device";
    let w = painter.layout_no_wrap(text.to_string(), theme::ui(14.0), Color32::WHITE).size().x;
    let plus = pos2(rect.center().x - w / 2.0 - 6.0, rect.center().y);
    let stroke = Stroke::new(1.5, c([0xC9, 0xCE, 0xD9]));
    painter.line_segment([plus - vec2(5.0, 0.0), plus + vec2(5.0, 0.0)], stroke);
    painter.line_segment([plus - vec2(0.0, 5.0), plus + vec2(0.0, 5.0)], stroke);
    painter.text(pos2(rect.center().x + 9.0, rect.center().y), Align2::CENTER_CENTER, text, theme::ui(14.0), c([0xC9, 0xCE, 0xD9]));
    r.clicked()
}

/// Points around a rounded rectangle, for a dashed frame.
fn rounded_outline(rect: Rect, radius: f32) -> Vec<Pos2> {
    let r = radius.min(rect.width() / 2.0).min(rect.height() / 2.0);
    let corners = [
        (pos2(rect.max.x - r, rect.min.y + r), -90.0f32),
        (pos2(rect.max.x - r, rect.max.y - r), 0.0),
        (pos2(rect.min.x + r, rect.max.y - r), 90.0),
        (pos2(rect.min.x + r, rect.min.y + r), 180.0),
    ];
    let mut points = vec![pos2(rect.min.x + r, rect.min.y)];
    for (center, start) in corners {
        for k in 0..=6 {
            let t = (start + 15.0 * k as f32).to_radians();
            points.push(center + vec2(t.cos(), t.sin()) * r);
        }
    }
    points.push(pos2(rect.min.x + r, rect.min.y));
    points
}

// The dialog.

/// Why an agent cannot build a device, or `None` when it can.
pub fn unable(core: &Core, id: &str) -> Option<&'static str> {
    unable_with(core.agent_dir(id).and_then(|d| d.config.runtime.as_ref()).map(|r| r.kind))
}

/// Why an agent on runtime `kind` cannot build a device. Mock agents only
/// play the scenario, external ones have no files here (D25).
fn unable_with(kind: Option<RuntimeKind>) -> Option<&'static str> {
    match kind {
        None | Some(RuntimeKind::Mock) => Some("mock, plays the demo"),
        Some(RuntimeKind::External) => Some("external, no files here"),
        Some(_) => None,
    }
}

/// The dialog while it is open: the picked agent and the description.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Ask {
    pub agent: Option<String>,
    pub text: String,
}

fn ask_key() -> Id {
    Id::new("ask-device-dialog")
}

fn recent_key() -> Id {
    Id::new("recent-agents")
}

/// Remembers the agent whose view shows, most recent first. ui/mod.rs calls
/// it every frame.
pub fn note_view(ctx: &egui::Context, view: &View) {
    let View::Agent(id) = view else { return };
    ctx.data_mut(|d| {
        let recent = d.get_temp_mut_or_default::<Vec<String>>(recent_key());
        if recent.first() != Some(id) {
            recent.retain(|r| r != id);
            recent.insert(0, id.clone());
            recent.truncate(16);
        }
    });
}

/// The agent the dialog starts with: the most recently viewed one that can
/// build a device, else the first such agent of the space.
pub fn default_agent(ctx: &egui::Context, core: &Core) -> Option<String> {
    let recent: Vec<String> = ctx.data(|d| d.get_temp(recent_key())).unwrap_or_default();
    let able = |id: &str| core.world.agent(id).is_some() && unable(core, id).is_none();
    first_able(&recent, core.world.agents.iter().map(|a| a.id.as_str()), able)
}

/// The first of the `recent` agents that is `able`, else the first such of all `agents`.
fn first_able<'a>(recent: &[String], mut agents: impl Iterator<Item = &'a str>, able: impl Fn(&str) -> bool) -> Option<String> {
    recent.iter().find(|id| able(id)).cloned().or_else(|| agents.find(|id| able(id)).map(String::from))
}

/// Opens the dialog with the default agent and `text`.
pub fn open(ctx: &egui::Context, core: &Core, text: &str) {
    let ask = Ask { agent: default_agent(ctx, core), text: text.to_string() };
    ctx.data_mut(|d| d.insert_temp(ask_key(), ask));
}

pub fn close(ctx: &egui::Context) {
    ctx.data_mut(|d| d.remove::<Ask>(ask_key()));
}

pub fn state(ctx: &egui::Context) -> Option<Ask> {
    ctx.data(|d| d.get_temp::<Ask>(ask_key()))
}

/// The agent picked in the open dialog. Its name tag wins (ui/tags.rs).
pub fn selected_agent(ctx: &egui::Context) -> Option<String> {
    state(ctx).and_then(|a| a.agent)
}

/// Where the agent daemon keeps the script API for agents to read inside
/// their sandbox (`script_api` in meerora-agentd's runs.rs).
pub fn api_path(data_dir: &std::path::Path) -> std::path::PathBuf {
    data_dir.join("docs").join("API.md")
}

/// The chat message that asks an agent for a device described in one line.
pub fn message(description: &str, api: &std::path::Path, space: &std::path::Path) -> String {
    let what = description.trim().trim_end_matches(['.', '!']).trim();
    format!(
        "Please build a device for my meerora space: {what}.\n\n\
A device is an object in the space with a shape, a live state binding and actions (D7). Write its script to devices/<name>.js in \
your directory, with a short lowercase file name. Read the device section of the script API in {api} first. The devices of the \
space in {examples} are working examples. Bind only sources you may read and use only capabilities you hold, and ask for missing \
ones with request_capability (D8). meerora tells you in this chat whether the device loaded. Once it loads, tell me in one \
sentence what it shows and what a click does.",
        api = api.display(),
        examples = space.join("devices").display(),
    )
}

/// Sends the request to `agent` and opens its view on the chat. Returns the
/// command, which goes to the daemon when it is connected.
pub fn request(core: &mut Core, agent: &str, description: &str) -> Command {
    let data_dir = core.assets.data_dir.clone();
    let text = message(description, &api_path(&data_dir), &meerora_protocol::space_dir(&data_dir));
    let command = Command::Say { agent: agent.to_string(), text };
    if let Some(link) = &core.link {
        link.send(&command);
    }
    let name = core.world.agent(agent).map_or_else(|| agent.to_string(), |a| a.name.clone());
    core.ui.toast(core.world.time, format!("Asked {name} to build a device"), false);
    open_agent(core, agent);
    core.ui.tab = AppTab::Chat;
    command
}

/// What the dialog asked for this frame.
enum Answer {
    Pick(String),
    Example(&'static str),
    Close,
}

/// The dialog next to the sidebar, above the button, while it is open.
pub fn draw_dialog(ui: &mut Ui, core: &mut Core, screen: Rect) {
    let Some(mut ask) = state(ui.ctx()) else { return };
    // An agent that left the space is no longer picked.
    if ask.agent.as_deref().is_some_and(|id| core.world.agent(id).is_none()) {
        ask.agent = default_agent(ui.ctx(), core);
    }
    let agents: Vec<(String, Option<&'static str>)> = core.world.agents.iter().map(|a| (a.id.clone(), unable(core, &a.id))).collect();
    let any_mock = agents.iter().any(|(_, u)| u.is_some());
    let any_able = agents.iter().any(|(_, u)| u.is_none());
    let connected = core.ui.daemon_connected;
    let inner = DIALOG_W - 40.0;
    let chip_w = (inner - 8.0) / 2.0;
    let grid_rows = agents.len().div_ceil(2);

    let painter = ui.painter();
    let intro = "The agent writes a device script in its directory. You approve what the device may do before it acts.";
    let intro_g = painter.layout(intro.to_string(), theme::ui(13.0), c(theme::TEXT_2), inner);
    let note = match (any_able, any_mock, connected) {
        (_, _, false) => Some("The agent daemon is not running, so no agent can answer yet."),
        (false, _, _) => Some("No agent here runs on a model. Give one a runtime in its builder, step 2."),
        (true, true, _) => Some("Mock agents only play the demo scenario, so they cannot build devices."),
        (true, false, _) => None,
    };
    let note_g = note.map(|n| painter.layout(n.to_string(), theme::ui(12.0), c(theme::TEXT_2), inner));
    // The examples flow in rows under the field.
    let mut examples: Vec<(&'static str, Rect)> = Vec::new();
    let mut at = Pos2::ZERO;
    for example in EXAMPLES {
        let w = painter.layout_no_wrap(example.to_string(), theme::ui(12.0), Color32::WHITE).size().x + 20.0;
        if at.x > 0.0 && at.x + w > inner {
            at = pos2(0.0, at.y + 34.0);
        }
        examples.push((example, Rect::from_min_size(at, vec2(w.min(inner), 26.0))));
        at.x += w + 8.0;
    }
    let examples_h = at.y + 26.0;
    let mut h = 20.0 + 24.0 + 8.0 + intro_g.size().y + 18.0 + 20.0 + grid_rows as f32 * 48.0;
    if let Some(g) = &note_g {
        h += 4.0 + g.size().y;
    }
    h += 18.0 + 20.0 + 42.0 + 10.0 + examples_h + 20.0 + 40.0 + 20.0;

    let opener = ui.ctx().data(|d| d.get_temp::<Rect>(Id::new("ask-device-button")));
    let left = sidebar_right() + 16.0;
    let bottom = opener.map_or(screen.max.y - 24.0, |b| b.max.y);
    let top = (bottom - h).clamp(HEADER_H + 16.0, (screen.max.y - 16.0 - h).max(HEADER_H + 16.0));
    let rect = Rect::from_min_size(pos2(left, top), vec2(DIALOG_W, h));

    let mut answer: Option<Answer> = None;
    let mut submit = false;
    egui::Area::new(Id::new("ask-device-area")).fade_in(false).fixed_pos(rect.min).order(Order::Foreground).show(ui.ctx(), |ui| {
        ui.allocate_exact_size(rect.size(), Sense::click());
        let painter = ui.painter();
        let shadow = egui::epaint::Shadow { offset: [0, 14], blur: 36, spread: 0, color: Color32::from_black_alpha(120) };
        painter.add(shadow.as_shape(rect, CornerRadius::same(16)));
        panel(painter, rect, 16);
        painter.rect_stroke(rect, CornerRadius::same(16), Stroke::new(1.0, c([0x2E, 0x36, 0x46])), StrokeKind::Inside);
        let x = rect.min.x + 20.0;
        let mut y = rect.min.y + 20.0;
        painter.text(pos2(x, y + 12.0), Align2::LEFT_CENTER, "Ask an agent to build a device", theme::ui_bold(16.5), c(theme::TEXT));
        let close = Rect::from_min_size(pos2(rect.max.x - 20.0 - 26.0, y), vec2(26.0, 26.0));
        if close_button(ui, close) {
            answer = Some(Answer::Close);
        }
        y += 24.0 + 8.0;
        let painter = ui.painter();
        let intro_h = intro_g.size().y;
        painter.galley(pos2(x, y), intro_g, c(theme::TEXT_2));
        y += intro_h + 18.0;

        label(painter, pos2(x, y), "AGENT");
        y += 20.0;
        for (k, (id, why)) in agents.iter().enumerate() {
            let at = pos2(x + (k % 2) as f32 * (chip_w + 8.0), y + (k / 2) as f32 * 48.0);
            let chip = Rect::from_min_size(at, vec2(chip_w, 40.0));
            let picked = ask.agent.as_deref() == Some(id.as_str());
            if agent_chip(ui, core, id, *why, chip, picked) {
                answer = Some(Answer::Pick(id.clone()));
            }
        }
        y += grid_rows as f32 * 48.0;
        if let Some(g) = note_g {
            y += 4.0;
            let gh = g.size().y;
            ui.painter().galley(pos2(x, y), g, c(theme::TEXT_2));
            y += gh;
        }
        y += 18.0;

        label(ui.painter(), pos2(x, y), "DEVICE");
        y += 20.0;
        let field = Rect::from_min_size(pos2(x, y), vec2(inner, 42.0));
        ui.painter().rect(
            field,
            CornerRadius::same(10),
            c([0x18, 0x1C, 0x25]),
            Stroke::new(1.0, c([0x2A, 0x30, 0x3D])),
            StrokeKind::Inside,
        );
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(field.shrink2(vec2(14.0, 11.0))));
        let resp = child.add(
            egui::TextEdit::singleline(&mut ask.text)
                .id(Id::new("ask-device-text"))
                .frame(egui::Frame::NONE)
                .hint_text(egui::RichText::new(EXAMPLES[0]).color(c([0x6B, 0x72, 0x82])))
                .font(theme::ui(14.5))
                .desired_width(field.width() - 28.0),
        );
        if !resp.has_focus() && ask.text.is_empty() && !ui.ctx().memory(|m| m.focused().is_some()) {
            resp.request_focus();
        }
        if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
            submit = true;
        }
        y += 42.0 + 10.0;
        // The examples fill the field.
        for (example, chip) in &examples {
            if example_chip(ui, chip.translate(vec2(x, y)), example) {
                answer = Some(Answer::Example(example));
            }
        }
        y += examples_h + 20.0;

        let ask_rect = Rect::from_min_size(pos2(rect.max.x - 20.0 - 96.0, y), vec2(96.0, 40.0));
        let cancel = Rect::from_min_size(pos2(ask_rect.min.x - 10.0 - 96.0, y), vec2(96.0, 40.0));
        let ready = ready(core, &ask);
        if ready {
            if button(ui, ask_rect, "Ask", ButtonStyle::Primary).clicked() {
                submit = true;
            }
        } else {
            disabled_button(ui.painter(), ask_rect, "Ask");
        }
        if button(ui, cancel, "Cancel", ButtonStyle::Quiet).clicked() {
            answer = Some(Answer::Close);
        }
    });
    if ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
        answer = Some(Answer::Close);
    }
    match answer {
        Some(Answer::Close) => {
            close(ui.ctx());
            return;
        }
        Some(Answer::Pick(id)) => ask.agent = Some(id),
        Some(Answer::Example(text)) => ask.text = text.to_string(),
        None => {}
    }
    if submit && ready(core, &ask) {
        let agent = ask.agent.clone().expect("a ready dialog has an agent");
        close(ui.ctx());
        request(core, &agent, &ask.text);
        return;
    }
    ui.ctx().data_mut(|d| d.insert_temp(ask_key(), ask));
}

/// Whether Ask can go: a picked agent that can build devices, a description
/// and the daemon to pass it on.
pub fn ready(core: &Core, ask: &Ask) -> bool {
    core.ui.daemon_connected
        && !ask.text.trim().is_empty()
        && ask.agent.as_deref().is_some_and(|id| core.world.agent(id).is_some() && unable(core, id).is_none())
}

/// One agent to pick: portrait, name and either its runtime or why it
/// cannot build devices, greyed out.
fn agent_chip(ui: &mut Ui, core: &Core, id: &str, why: Option<&str>, rect: Rect, picked: bool) -> bool {
    let able = why.is_none();
    let r = ui.interact(rect, Id::new(("ask-device-agent", id)), if able { Sense::click() } else { Sense::hover() });
    let painter = ui.painter();
    let (fill, border) = match (picked, able && r.hovered()) {
        (true, _) => (ca(theme::NEEDS_YOU, 30), ca(theme::NEEDS_YOU, 200)),
        (false, true) => (c([0x1E, 0x23, 0x2E]), c([0x3A, 0x42, 0x52])),
        (false, false) => (c([0x16, 0x1A, 0x23]), c([0x23, 0x28, 0x34])),
    };
    painter.rect(rect, CornerRadius::same(10), fill, Stroke::new(1.0, border), StrokeKind::Inside);
    let Some(a) = core.world.agent(id) else { return false };
    let face = Rect::from_center_size(pos2(rect.min.x + 20.0, rect.center().y), vec2(30.0, 30.0));
    portrait(painter, face, core, a);
    if !able {
        painter.rect_filled(face, CornerRadius::same(4), ca([0x16, 0x1A, 0x23], 150));
    }
    let text_w = rect.width() - 46.0;
    let name_color = if able { c(theme::TEXT) } else { c([0x6B, 0x72, 0x82]) };
    let name = fit(painter, &a.name, theme::ui_semibold(13.5), text_w);
    painter.text(pos2(rect.min.x + 40.0, rect.min.y + 5.0), Align2::LEFT_TOP, name, theme::ui_semibold(13.5), name_color);
    let sub = match why {
        Some(why) => why.to_string(),
        None => core.agent_dir(id).and_then(|d| d.config.runtime.as_ref()).map(|r| runtime_name(r.kind).to_string()).unwrap_or_default(),
    };
    let sub_color = if able { c(theme::TEXT_2) } else { c([0x5B, 0x61, 0x70]) };
    painter.text(
        pos2(rect.min.x + 40.0, rect.min.y + 22.0),
        Align2::LEFT_TOP,
        fit(painter, &sub, theme::mono(10.0), text_w),
        theme::mono(10.0),
        sub_color,
    );
    if able && r.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    able && r.clicked()
}

fn runtime_name(kind: RuntimeKind) -> &'static str {
    match kind {
        RuntimeKind::ClaudeCli => "claude cli",
        RuntimeKind::CodexCli => "codex cli",
        RuntimeKind::Ollama => "ollama",
        RuntimeKind::Api => "api",
        RuntimeKind::External => "external",
        RuntimeKind::Mock => "mock",
    }
}

fn example_chip(ui: &mut Ui, rect: Rect, text: &str) -> bool {
    let r = ui.interact(rect, Id::new(("ask-device-example", text)), Sense::click());
    let painter = ui.painter();
    let border = if r.hovered() { c([0x3A, 0x42, 0x52]) } else { c([0x2A, 0x30, 0x3D]) };
    let fill = if r.hovered() { c([0x1A, 0x1F, 0x2A]) } else { Color32::TRANSPARENT };
    painter.rect(rect, CornerRadius::same(13), fill, Stroke::new(1.0, border), StrokeKind::Inside);
    painter.text(rect.center(), Align2::CENTER_CENTER, text, theme::ui(12.0), c([0xC9, 0xCE, 0xD9]));
    if r.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    r.clicked()
}

fn disabled_button(painter: &Painter, rect: Rect, label: &str) {
    painter.rect(rect, CornerRadius::same(10), c([0x1A, 0x1F, 0x2A]), Stroke::new(1.0, c([0x23, 0x28, 0x34])), StrokeKind::Inside);
    painter.text(rect.center(), Align2::CENTER_CENTER, label, theme::ui_bold(14.5), c([0x5B, 0x61, 0x70]));
}

fn close_button(ui: &mut Ui, rect: Rect) -> bool {
    let r = ui.interact(rect, Id::new("ask-device-close"), Sense::click());
    let painter = ui.painter();
    if r.hovered() {
        painter.rect_filled(rect, CornerRadius::same(6), c([0x1A, 0x1F, 0x2A]));
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let k = 4.5;
    for (a, b) in [(vec2(-k, -k), vec2(k, k)), (vec2(-k, k), vec2(k, -k))] {
        painter.line_segment([rect.center() + a, rect.center() + b], Stroke::new(1.5, c(theme::TEXT_2)));
    }
    r.clicked()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    #[test]
    fn mock_and_external_agents_cannot_build_devices() {
        assert!(unable_with(None).is_some());
        assert!(unable_with(Some(RuntimeKind::Mock)).is_some());
        assert!(unable_with(Some(RuntimeKind::External)).is_some());
        for kind in [RuntimeKind::ClaudeCli, RuntimeKind::CodexCli, RuntimeKind::Ollama, RuntimeKind::Api] {
            assert_eq!(unable_with(Some(kind)), None, "{kind:?}");
        }
    }

    #[test]
    fn the_dialog_starts_with_the_agent_viewed_last_that_can_build() {
        let able = |id: &str| ["claude", "ember"].contains(&id);
        let agents = ["moth", "claude", "ember"];
        let recent = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(first_able(&recent(&["moth", "ember", "claude"]), agents.into_iter(), able).as_deref(), Some("ember"));
        assert_eq!(first_able(&recent(&[]), agents.into_iter(), able).as_deref(), Some("claude"), "else the first that can");
        assert_eq!(first_able(&recent(&["moth"]), ["moth"].into_iter(), able), None, "mock agents only");
    }

    #[test]
    fn the_request_explains_the_device_and_points_at_the_api() {
        let text = message("  a lamp that glows while CI runs. ", Path::new("/data/docs/API.md"), Path::new("/data/spaces/workshop"));
        assert!(text.starts_with("Please build a device for my meerora space: a lamp that glows while CI runs.\n"), "{text}");
        for needle in ["devices/<name>.js", "/data/docs/API.md", "/data/spaces/workshop/devices", "request_capability", "loaded"] {
            assert!(text.contains(needle), "missing {needle}: {text}");
        }
        assert_eq!(api_path(Path::new("/data")), PathBuf::from("/data/docs/API.md"), "the path the daemon writes (runs.rs)");
    }

    fn copy_dir(from: &Path, to: &Path) {
        std::fs::create_dir_all(to).unwrap();
        for entry in std::fs::read_dir(from).unwrap().flatten() {
            let target = to.join(entry.file_name());
            if entry.path().is_dir() {
                copy_dir(&entry.path(), &target);
            } else {
                std::fs::copy(entry.path(), target).unwrap();
            }
        }
    }

    const LAMP: &str = r##"
const base = cylinder(7, 2, 12).material("metal", "#3B404C");
const lit = cone(9, 10, 12, 4).material("enamel", "#F2B45A").glow(1).on(base);
export default device({
  name: "Focus lamp",
  parts: [base],
  params: { on: toggle(false).shows(lit, true) },
  place: { station: "workbench", offset: [0, 92, -40] },
  bind: { on: from("agent.ember.ring").is("working") },
  status: { on: { true: "On while Ember works.", false: "Off. Ember rests." } },
});
"##;

    fn info(id: &str, owner: Option<&str>, granted: &[&str]) -> meerora_protocol::DeviceInfo {
        meerora_protocol::DeviceInfo {
            id: id.into(),
            name: id.into(),
            owner: owner.map(String::from),
            script: String::new(),
            revision: String::new(),
            actions: Vec::new(),
            needs: Vec::new(),
            granted: granted.iter().map(|g| g.to_string()).collect(),
            error: None,
        }
    }

    /// The real app on a headless GPU in its own copy of the assets, where
    /// Claude runs on the claude CLI and Rivet made a lamp. The devices
    /// list has its rows, and the dialog sends Claude the right Say over a
    /// socket like the daemon's.
    #[test]
    fn the_devices_list_and_the_dialog_send_the_request() {
        let gpu = match meerora_scene::Gpu::headless() {
            Ok(gpu) => gpu,
            Err(e) => {
                eprintln!("note: skipping, no GPU adapter ({e:#})");
                return;
            }
        };
        let assets = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets");
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().subsec_nanos();
        let data = std::env::temp_dir().join(format!("meerora-device-list-{}-{nanos}", std::process::id()));
        copy_dir(&assets.join("spaces"), &data.join("spaces"));
        copy_dir(&assets.join("agents"), &data.join("agents"));
        let toml = data.join("agents/claude/agent.toml");
        std::fs::write(&toml, std::fs::read_to_string(&toml).unwrap().replace("kind = \"mock\"", "kind = \"claude-cli\"")).unwrap();
        std::fs::create_dir_all(data.join("agents/rivet/devices")).unwrap();
        std::fs::write(data.join("agents/rivet/devices/lamp.js"), LAMP).unwrap();

        let (device, queue) = (&gpu.device, &gpu.queue);
        let mut core =
            Core::new(device, meerora_scene::IMAGE_FORMAT, crate::assets::Assets::without_watcher(data.clone()), None, Arc::new(|| {}));
        core.load(device, queue);

        // Rows: the space's devices by name, then Rivet's lamp once the daemon loaded it (D8).
        let names = |core: &Core| rows(core).into_iter().map(|r| r.name).collect::<Vec<_>>();
        assert!(!names(&core).contains(&"Focus lamp".to_string()), "an agent's device waits for the daemon");
        let mut infos: Vec<meerora_protocol::DeviceInfo> =
            core.devices.items.iter().filter(|d| d.owner.is_none()).map(|d| info(&d.id, None, &[])).collect();
        infos.push(info("rivet/lamp", Some("rivet"), &[]));
        core.apply(&meerora_protocol::Event::Devices { devices: infos.clone() });
        core.apply(&meerora_protocol::Event::DeviceCapabilityRequest {
            id: "cap-1".into(),
            device: "airplane".into(),
            capability: "network.airplane_mode.write".into(),
            action: "Toggle airplane mode".into(),
        });
        core.step(device, queue, 1.0 / 30.0);
        let list = rows(&core);
        assert_eq!(names(&core), ["Airplane mode", "Clock", "Mailbox", "Noticeboard", "System graph", "Tree", "Focus lamp"]);
        let row = |name: &str| list.iter().find(|r| r.name == name).unwrap().clone();
        assert_eq!(row("Airplane mode").lock, Lock::Pending, "it waits for the user's answer");
        assert_eq!(row("Airplane mode").glyph, Glyph::Plane);
        assert!(row("Airplane mode").state.starts_with("Off."), "{}", row("Airplane mode").state);
        assert_eq!(row("Clock").lock, Lock::None);
        let lamp = row("Focus lamp");
        assert_eq!(lamp.owner, Some(("rivet".to_string(), "Rivet".to_string())));
        assert_eq!(lamp.state, "by Rivet · Off. Ember rests.");
        core.ui.approvals.devices.clear();
        assert_eq!(rows(&core)[0].lock, Lock::Asks, "unanswered, it asks at the first click");
        infos[0] = info("airplane", None, &["network.airplane_mode.write"]);
        core.apply(&meerora_protocol::Event::Devices { devices: infos });
        assert_eq!(rows(&core)[0].lock, Lock::Held(1), "allowed, it shows a padlock");

        // The dialog over a socket that plays the daemon.
        let socket = data.join("agentd.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let link = crate::daemon::DaemonLink::start(socket.clone(), &data, false, || {});
        let (stream, _) = listener.accept().unwrap();
        stream.set_read_timeout(Some(std::time::Duration::from_secs(10))).unwrap();
        let mut lines = std::io::BufRead::lines(std::io::BufReader::new(stream));
        let hello = lines.next().unwrap().unwrap();
        assert!(hello.contains("hello"), "{hello}");
        assert!(matches!(link.rx.recv_timeout(std::time::Duration::from_secs(10)), Ok(crate::daemon::LinkMsg::Connected)));
        core.link = Some(link);
        core.ui.daemon_connected = true;

        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        let screen = Rect::from_min_size(Pos2::ZERO, vec2(1440.0, 900.0));
        let mut time = 0.0;
        let mut frame = |core: &mut Core, events: Vec<egui::Event>| {
            time += 0.05;
            let raw = egui::RawInput { screen_rect: Some(screen), time: Some(time), events, ..Default::default() };
            let mut output = ctx.run_ui(raw, |ui| core.build_ui(ui));
            output.textures_delta.clear();
        };
        frame(&mut core, Vec::new());
        open(&ctx, &core, "");
        assert_eq!(state(&ctx).and_then(|a| a.agent).as_deref(), Some("claude"), "the only agent on a real runtime");
        assert_eq!(selected_agent(&ctx).as_deref(), Some("claude"), "its tag wins");
        frame(&mut core, Vec::new());
        frame(&mut core, vec![egui::Event::Text(EXAMPLES[0].to_string())]);
        assert_eq!(state(&ctx).map(|a| a.text).as_deref(), Some(EXAMPLES[0]));
        let enter =
            egui::Event::Key { key: egui::Key::Enter, physical_key: None, pressed: true, repeat: false, modifiers: egui::Modifiers::NONE };
        frame(&mut core, vec![enter]);
        assert_eq!(state(&ctx), None, "the dialog closed");
        assert_eq!(core.ui.view, View::Agent("claude".into()), "Claude's view opens to follow");
        assert_eq!(core.ui.tab, AppTab::Chat);
        // Other commands may come first, such as the app's own announcements.
        let said = lines
            .map(|line| meerora_protocol::decode_command(&line.unwrap()).unwrap())
            .find_map(|c| match c {
                Command::Say { agent, text } => Some((agent, text)),
                _ => None,
            })
            .expect("a Say");
        assert_eq!(said.0, "claude");
        assert_eq!(said.1, message(EXAMPLES[0], &api_path(&data), &meerora_protocol::space_dir(&data)));
        drop(core);
        std::fs::remove_dir_all(&data).unwrap();
    }
}
