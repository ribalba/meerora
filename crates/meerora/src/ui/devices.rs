//! Devices in the space view (D7, D8): hover, the capability badge on the
//! object and the device card.
//!
//! A click on a device opens its card next to it: the name, the state its
//! binding shows, who owns it, its declared capabilities with a padlock and
//! the action buttons. The scene turns on a drag and opens the card on a
//! click, the same way it tells clicks on agents from drags. A click on an
//! action sends `DeviceAction`. When a device of the space lacks the
//! capability, the daemon asks with the approval card of `approvals.rs`.
//! A device with settings, such as the metric of the system graph, shows a
//! row of choices for each, and a click sends `DeviceSetting`.

use egui::text::LayoutJob;
use egui::{Align2, Color32, CornerRadius, Id, LayerId, Order, Painter, Pos2, Rect, Sense, Shape, Stroke, StrokeKind, Ui, pos2, vec2};
use meerora_geom::{Vec2 as GVec2, Vec3};
use meerora_protocol::{Command, DeviceActionInfo};
use meerora_script::Source;

use super::approvals::lock;
use super::space::{HEADER_H, sidebar_right};
use super::{ButtonStyle, button, fit, pill};
use crate::devices::{Device, transform};
use crate::engine::Core;
use crate::theme::{self, c, ca};

const CARD_W: f32 = 352.0;

/// The device under the pointer when it is nearer than the agent the world
/// picked. `agent` is that agent's index.
pub fn pick(core: &Core, camera: &meerora_scene::Camera, origin: Vec3, dir: Vec3, agent: Option<usize>) -> Option<usize> {
    let (device, t) = core.devices.pick(&core.world, camera.toward_viewer(), origin, dir)?;
    let agent_t = agent.and_then(|i| core.world.bounds(&core.world.agents[i]).ray_hit(origin, dir));
    agent_t.is_none_or(|a| t < a).then_some(device)
}

/// Marks the hovered device, opens or closes the card after a click.
pub fn after_pick(core: &mut Core, hovered: Option<usize>, clicked: bool) {
    for (i, d) in core.devices.items.iter_mut().enumerate() {
        d.hover = Some(i) == hovered;
    }
    if clicked {
        core.devices.card = hovered.map(|i| core.devices.items[i].id.clone());
    }
}

/// Where a device is on screen, the middle of the top of its bounds.
fn anchor(core: &Core, d: &Device, camera: &meerora_scene::Camera, viewport: GVec2) -> Option<Pos2> {
    let m = transform(&core.world, d)?;
    let b = crate::devices::visible_boxes(d, m, core.world.time as f32).into_iter().reduce(|a, b| a.union(b))?;
    let top = Vec3::new((b.min.x + b.max.x) / 2.0, b.max.y, (b.min.z + b.max.z) / 2.0);
    camera.project(top, viewport).map(|p| pos2(p.x, p.y))
}

/// The badges, the hover label and the card.
pub fn draw(ui: &mut Ui, core: &mut Core, camera: &meerora_scene::Camera, viewport: GVec2, screen: Rect) {
    draw_badges(ui, core, camera, viewport);
    let Some(id) = core.devices.card.clone() else { return };
    let Some(d) = core.devices.device(&id).filter(|d| core.devices.shown(d)) else {
        core.devices.card = None;
        return;
    };
    let toward = camera.toward_viewer();
    let seen = if d.desc.place.shown(toward) { anchor(core, d, camera, viewport).filter(|p| screen.contains(*p)) } else { None };
    // A card opened from the devices list in the sidebar shows next to its
    // row while its device is out of sight (device_list.rs).
    let at = match (seen, super::device_list::row_anchor(ui.ctx(), &id)) {
        (Some(at), _) => at,
        (None, Some(row)) => row,
        (None, None) if !d.desc.place.shown(toward) => {
            // The wall it hangs on was cut away, and the card goes with it.
            core.devices.card = None;
            return;
        }
        (None, None) => anchor(core, d, camera, viewport).unwrap_or(screen.center()),
    };
    draw_card(ui, core, &id, at, screen);
}

/// A padlock above every device that holds a capability (D8), and the name
/// of the hovered device.
fn draw_badges(ui: &mut Ui, core: &Core, camera: &meerora_scene::Camera, viewport: GVec2) {
    let painter = ui.ctx().layer_painter(LayerId::new(Order::Background, Id::new("device-badges")));
    let toward = camera.toward_viewer();
    for (_, d, _) in core.devices.placed(&core.world) {
        if !d.desc.place.shown(toward) {
            continue;
        }
        let Some(at) = anchor(core, d, camera, viewport) else { continue };
        let granted = core.devices.info(&d.id).map(|i| i.granted.len()).unwrap_or(0);
        let mut y = at.y - 8.0;
        if granted > 0 {
            let rect = Rect::from_center_size(pos2(at.x, y - 11.0), vec2(if granted > 1 { 36.0 } else { 24.0 }, 22.0));
            painter.rect(
                rect,
                CornerRadius::same(7),
                ca([0x16, 0x14, 0x28], 235),
                Stroke::new(1.0, ca(theme::SYSTEM, 170)),
                StrokeKind::Inside,
            );
            let lock_at = if granted > 1 { pos2(rect.min.x + 12.0, rect.center().y) } else { rect.center() };
            lock(&painter, lock_at, 12.0, c(theme::SYSTEM_2));
            if granted > 1 {
                painter.text(
                    pos2(rect.max.x - 9.0, rect.center().y),
                    Align2::CENTER_CENTER,
                    granted.to_string(),
                    theme::mono(10.5),
                    c(theme::SYSTEM_2),
                );
            }
            y = rect.min.y - 4.0;
        }
        if d.hover && core.devices.card.as_deref() != Some(&d.id) {
            let galley = painter.layout_no_wrap(d.desc.name.clone(), theme::ui_semibold(13.0), c(theme::TEXT));
            let rect = Rect::from_min_size(pos2(at.x - galley.size().x / 2.0 - 10.0, y - 26.0), vec2(galley.size().x + 20.0, 24.0));
            painter.rect(
                rect,
                CornerRadius::same(7),
                ca([0x12, 0x16, 0x1E], 236),
                Stroke::new(1.0, c([0x2A, 0x30, 0x3D])),
                StrokeKind::Inside,
            );
            painter.galley(rect.min + vec2(10.0, 12.0 - galley.size().y / 2.0), galley, c(theme::TEXT));
        }
    }
}

/// What a click on a device card asked for.
enum Click {
    Action(DeviceActionInfo),
    Revoke(String),
    /// A setting and the value picked for it.
    Setting(String, serde_json::Value),
    Close,
}

/// A setting of a device as its card shows it: the param, the label, the
/// values with their labels and the current value.
struct SettingRow {
    param: String,
    label: String,
    values: Vec<(serde_json::Value, String)>,
    current: Option<serde_json::Value>,
}

fn setting_rows(d: &Device) -> Vec<SettingRow> {
    d.desc
        .settings
        .iter()
        .filter_map(|s| {
            let def = d.desc.param(&s.param)?;
            let values: Vec<serde_json::Value> = match &def.kind {
                meerora_script::ParamKind::Choice { values, .. } => values.iter().map(|v| serde_json::Value::from(v.as_str())).collect(),
                meerora_script::ParamKind::Toggle { .. } => vec![false.into(), true.into()],
                _ => return None,
            };
            Some(SettingRow {
                param: s.param.clone(),
                label: s.label.clone(),
                values: values.into_iter().map(|v| (v.clone(), d.desc.setting_label(&s.param, &v))).collect(),
                current: d.value(&s.param),
            })
        })
        .collect()
}

fn draw_card(ui: &mut Ui, core: &mut Core, id: &str, at: Pos2, screen: Rect) {
    let Some(d) = core.devices.device(id) else { return };
    let info = core.devices.info(id).cloned();
    let connected = core.ui.daemon_connected;
    let status = d.desc.status_text(&d.params);
    // What it follows now: a setting may switch the bindings (`per()`).
    let sources: Vec<String> =
        d.desc.active_bindings(&d.params).map(|b| b.source.to_string()).collect::<std::collections::BTreeSet<_>>().into_iter().collect();
    let owner = match &d.owner {
        Some(agent) => core.world.agent(agent).map_or(agent.clone(), |a| a.name.clone()),
        None => String::new(),
    };
    // Actions come from the daemon, which checked them. Without it the
    // card lists what the script declares, and they wait for the daemon.
    let actions: Vec<DeviceActionInfo> = match &info {
        Some(info) => info.actions.clone(),
        None => d
            .desc
            .actions
            .iter()
            .map(|a| DeviceActionInfo {
                id: a.id.clone(),
                label: a.label.clone(),
                capability: a.capability().map(String::from),
                opens: match &a.kind {
                    meerora_script::ActionKind::Opens(view) => Some(view.clone()),
                    meerora_script::ActionKind::Capability { .. } => None,
                },
            })
            .collect(),
    };
    // A device of the space reads its sources freely, only its actions need
    // the user's approval. An agent's device needs all of them (D8).
    let needs: Vec<String> = if d.owner.is_none() {
        let mut caps: Vec<String> = actions.iter().filter_map(|a| a.capability.clone()).collect();
        caps.dedup();
        caps
    } else {
        info.as_ref().map(|i| i.needs.clone()).unwrap_or_else(|| d.desc.capabilities())
    };
    let granted: Vec<String> = info.as_ref().map(|i| i.granted.clone()).unwrap_or_default();
    let glyph = glyph_for(d);
    let name = d.desc.name.clone();
    let space_device = d.owner.is_none();
    let settings = setting_rows(d);

    let inner = CARD_W - 32.0;
    let painter = ui.painter();
    let status_galley = status.as_ref().map(|s| paragraph(painter, s, theme::ui(14.0), c([0xDD, 0xE1, 0xEA]), inner, 3));
    let mut h = 16.0 + 26.0;
    if let Some(g) = &status_galley {
        h += 8.0 + g.size().y;
    }
    if !sources.is_empty() {
        h += 8.0 + 16.0;
    }
    h += settings.len() as f32 * (10.0 + 16.0 + 32.0);
    h += 6.0 + 16.0; // the owner line
    h += needs.len() as f32 * 32.0;
    if !needs.is_empty() {
        h += 6.0;
    }
    if !actions.is_empty() {
        h += 12.0 + 40.0;
    }
    let waits = !connected && actions.iter().any(|a| a.capability.is_some());
    if waits {
        h += 8.0 + 16.0;
    }
    h += 16.0;

    // The card goes on the side of the device with more room, like the
    // card of screen 1 left of the plane.
    let left = sidebar_right() + 16.0;
    let fits_left = at.x - 36.0 - CARD_W >= left;
    let fits_right = at.x + 36.0 + CARD_W <= screen.max.x - 24.0;
    let middle = (left + screen.max.x - 24.0) / 2.0;
    let x = match (fits_left, fits_right) {
        (true, true) if at.x < middle => at.x - 36.0 - CARD_W,
        (_, true) => at.x + 36.0,
        (true, false) => at.x - 36.0 - CARD_W,
        (false, false) => left,
    };
    let x = x.min(screen.max.x - 24.0 - CARD_W).max(left);
    let y = (at.y - h * 0.35).clamp(HEADER_H + 16.0, (screen.max.y - 140.0 - h).max(HEADER_H + 16.0));
    let rect = Rect::from_min_size(pos2(x, y), vec2(CARD_W, h));

    let mut click = None;
    egui::Area::new(Id::new("device-card")).fade_in(false).fixed_pos(rect.min).order(Order::Middle).show(ui.ctx(), |ui| {
        ui.allocate_exact_size(rect.size(), Sense::click());
        let painter = ui.painter();
        let shadow = egui::epaint::Shadow { offset: [0, 14], blur: 36, spread: 0, color: Color32::from_black_alpha(120) };
        painter.add(shadow.as_shape(rect, CornerRadius::same(16)));
        painter.rect(rect, CornerRadius::same(16), c(theme::PANEL), Stroke::new(1.0, c([0x2A, 0x30, 0x3D])), StrokeKind::Inside);
        let x = rect.min.x + 16.0;
        let mut y = rect.min.y + 16.0;

        // Title row: the glyph, the name and who owns it.
        draw_glyph(painter, glyph, pos2(x + 11.0, y + 13.0), 1.0);
        let chip = if space_device { "YOUR SPACE".to_string() } else { format!("BY {}", owner.to_uppercase()) };
        let close = Rect::from_min_size(pos2(rect.max.x - 16.0 - 24.0, y + 1.0), vec2(24.0, 24.0));
        let pill_rect =
            pill(painter, pos2(close.min.x - 6.0, y + 13.0), &chip, if space_device { theme::NEEDS_YOU } else { theme::SYSTEM_2 });
        let title = fit(painter, &name, theme::ui_bold(15.5), pill_rect.min.x - x - 34.0);
        painter.text(pos2(x + 30.0, y + 13.0), Align2::LEFT_CENTER, title, theme::ui_bold(15.5), c(theme::TEXT));
        let r = ui.interact(close, Id::new(("device-card-close", rect.min.x as i32)), Sense::click());
        let painter = ui.painter();
        if r.hovered() {
            painter.rect_filled(close, CornerRadius::same(6), c([0x1A, 0x1F, 0x2A]));
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        let k = 4.5;
        for (a, b) in [(vec2(-k, -k), vec2(k, k)), (vec2(-k, k), vec2(k, -k))] {
            painter.line_segment([close.center() + a, close.center() + b], Stroke::new(1.5, c(theme::TEXT_2)));
        }
        if r.clicked() {
            click = Some(Click::Close);
        }
        y += 26.0;

        if let Some(g) = status_galley {
            y += 8.0;
            let gh = g.size().y;
            painter.galley(pos2(x, y), g, c([0xDD, 0xE1, 0xEA]));
            y += gh;
        }
        if !sources.is_empty() {
            y += 8.0;
            let text = fit(painter, &format!("follows {}", sources.join(", ")), theme::mono(11.0), inner);
            painter.text(pos2(x, y), Align2::LEFT_TOP, text, theme::mono(11.0), c(theme::TEXT_2));
            y += 16.0;
        }
        // Settings: a label and a row of choices, the current one lit.
        for row in &settings {
            y += 10.0;
            ui.painter().text(pos2(x, y), Align2::LEFT_TOP, row.label.to_uppercase(), theme::mono(10.5), c(theme::TEXT_2));
            y += 16.0;
            let gap = 6.0;
            let w = (inner - gap * (row.values.len() as f32 - 1.0)) / row.values.len().max(1) as f32;
            for (k, (value, label)) in row.values.iter().enumerate() {
                let b = Rect::from_min_size(pos2(x + k as f32 * (w + gap), y), vec2(w, 28.0));
                let chosen = row.current.as_ref() == Some(value);
                if choice_button(ui, b, label, chosen).clicked() && !chosen {
                    click = Some(Click::Setting(row.param.clone(), value.clone()));
                }
            }
            y += 32.0;
        }
        let painter = ui.painter();
        y += 6.0;
        let who = match (space_device, needs.is_empty()) {
            (true, true) => "Part of your space.".to_string(),
            (true, false) => "Part of your space. You approve what it may do.".to_string(),
            (false, _) => format!("Made by {owner}. It uses what {owner} holds."),
        };
        painter.text(pos2(x, y), Align2::LEFT_TOP, fit(painter, &who, theme::ui(12.5), inner), theme::ui(12.5), c(theme::TEXT_2));
        y += 16.0;
        if !needs.is_empty() {
            y += 6.0;
        }
        for capability in &needs {
            let painter = ui.painter();
            let row = Rect::from_min_size(pos2(x, y), vec2(inner, 26.0));
            painter.rect_filled(row, CornerRadius::same(8), c([0x1B, 0x20, 0x30]));
            lock(painter, pos2(row.min.x + 13.0, row.center().y), 11.0, c(theme::SYSTEM_2));
            let held = granted.contains(capability);
            let (state, color) = match (held, space_device) {
                (true, true) => ("allowed", theme::SYSTEM_2),
                (true, false) => ("held", theme::SYSTEM_2),
                (false, true) => ("asks first", theme::TEXT_2),
                (false, false) => ("missing", [0xE0, 0x67, 0x5A]),
            };
            let revoke = held && space_device && connected;
            let state_right = if revoke { row.max.x - 70.0 } else { row.max.x - 8.0 };
            let state_w = painter.layout_no_wrap(state.to_string(), theme::mono(10.0), c(color)).size().x;
            painter.text(pos2(state_right, row.center().y), Align2::RIGHT_CENTER, state, theme::mono(10.0), c(color));
            let cap = fit(painter, capability, theme::mono(10.5), state_right - state_w - 8.0 - (row.min.x + 24.0));
            painter.text(pos2(row.min.x + 24.0, row.center().y), Align2::LEFT_CENTER, cap, theme::mono(10.5), c([0xC9, 0xCE, 0xD9]));
            if revoke {
                let b = Rect::from_min_size(pos2(row.max.x - 62.0, row.min.y + 2.0), vec2(60.0, 22.0));
                if small_button(ui, b, "Revoke").clicked() {
                    click = Some(Click::Revoke(capability.clone()));
                }
            }
            y += 32.0;
        }
        if !actions.is_empty() {
            y += 12.0;
            let gap = 8.0;
            let w = (inner - gap * (actions.len() as f32 - 1.0)) / actions.len() as f32;
            for (k, action) in actions.iter().enumerate() {
                let b = Rect::from_min_size(pos2(x + k as f32 * (w + gap), y), vec2(w, 40.0));
                let style = if k == 0 && (connected || action.capability.is_none()) { ButtonStyle::Primary } else { ButtonStyle::Quiet };
                if button(ui, b, &action.label, style).clicked() {
                    click = Some(Click::Action(action.clone()));
                }
            }
            y += 40.0;
        }
        if waits {
            y += 8.0;
            ui.painter().text(pos2(x, y), Align2::LEFT_TOP, "The agent daemon is not running.", theme::ui(12.5), c([0xE0, 0x67, 0x5A]));
        }
    });
    let now = core.world.time;
    match click {
        Some(Click::Close) => core.devices.card = None,
        Some(Click::Revoke(capability)) => {
            if let Some(link) = &core.link {
                link.send(&Command::RevokeDeviceCapability { device: id.into(), capability });
            }
        }
        Some(Click::Setting(name, value)) => match &core.link {
            // The daemon keeps the choice and sends it back with the values that follow it.
            Some(link) if connected => link.send(&Command::DeviceSetting { device: id.into(), name, value }),
            _ => core.ui.toast(now, "The agent daemon is not running, so the device cannot switch yet.", true),
        },
        Some(Click::Action(action)) => match (&action.opens, &action.capability) {
            (Some(view), _) => open_view(core, view),
            (None, Some(_)) if !connected => core.ui.toast(now, "The agent daemon is not running, so devices do not act yet.", true),
            (None, _) => {
                if let Some(link) = &core.link {
                    link.send(&Command::DeviceAction { device: id.into(), action: action.id.clone() });
                }
            }
        },
        None => {}
    }
    // Esc closes the card.
    if ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
        core.devices.card = None;
    }
}

/// Opens a view of the app for an `opens()` action. Only the feed so far (D27).
fn open_view(core: &mut Core, view: &str) {
    core.devices.card = None;
    match view {
        "feed" => super::feed::open(core),
        other => core.ui.toast(core.world.time, format!("meerora has no view called {other}"), true),
    }
}

/// One choice of a setting row. The chosen one is filled.
fn choice_button(ui: &mut Ui, rect: Rect, label: &str, chosen: bool) -> egui::Response {
    let resp = ui.interact(rect, Id::new(("device-choice", label, rect.min.x as i32, rect.min.y as i32)), Sense::click());
    let painter = ui.painter();
    let (fill, border, text) = if chosen {
        (ca(theme::NEEDS_YOU, 40), ca(theme::NEEDS_YOU, 200), c(theme::NEEDS_YOU))
    } else if resp.hovered() {
        (c([0x1E, 0x23, 0x2E]), c([0x3A, 0x42, 0x52]), c([0xDD, 0xE1, 0xEA]))
    } else {
        (Color32::TRANSPARENT, c([0x2E, 0x36, 0x46]), c([0xC9, 0xCE, 0xD9]))
    };
    painter.rect(rect, CornerRadius::same(7), fill, Stroke::new(1.0, border), StrokeKind::Inside);
    let label = fit(painter, label, theme::ui(12.5), rect.width() - 8.0);
    painter.text(rect.center(), Align2::CENTER_CENTER, label, theme::ui(12.5), text);
    if resp.hovered() && !chosen {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp
}

fn small_button(ui: &mut Ui, rect: Rect, label: &str) -> egui::Response {
    let resp = ui.interact(rect, Id::new(("device-small", label, rect.min.x as i32, rect.min.y as i32)), Sense::click());
    let painter = ui.painter();
    let border = if resp.hovered() { c([0x3A, 0x42, 0x52]) } else { c([0x2E, 0x36, 0x46]) };
    painter.rect(
        rect,
        CornerRadius::same(6),
        if resp.hovered() { c([0x1E, 0x23, 0x2E]) } else { Color32::TRANSPARENT },
        Stroke::new(1.0, border),
        StrokeKind::Inside,
    );
    painter.text(rect.center(), Align2::CENTER_CENTER, label, theme::ui(12.0), c([0xC9, 0xCE, 0xD9]));
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp
}

fn paragraph(painter: &Painter, text: &str, font: egui::FontId, color: Color32, width: f32, rows: usize) -> std::sync::Arc<egui::Galley> {
    let mut job = LayoutJob::simple(text.to_string(), font, color, width);
    job.wrap.max_rows = rows;
    job.wrap.overflow_character = Some('…');
    painter.layout_job(job)
}

/// The small picture in front of a device's name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Glyph {
    Plane,
    Clock,
    Tree,
    Flag,
    Board,
    Graph,
}

pub fn glyph_for(d: &Device) -> Glyph {
    if d.desc.actions.iter().any(|a| matches!(a.kind, meerora_script::ActionKind::Opens(_))) {
        return Glyph::Board;
    }
    if d.desc.display.is_some() {
        return Glyph::Graph;
    }
    match d.desc.bindings.first().map(|b| &b.source) {
        Some(Source::AirplaneMode) => Glyph::Plane,
        Some(Source::Clock) => Glyph::Clock,
        Some(Source::Disk | Source::Memory) => Glyph::Tree,
        Some(Source::Cpu | Source::Network) => Glyph::Graph,
        Some(Source::AgentParam { .. } | Source::AgentRing { .. }) => Glyph::Flag,
        None => Glyph::Board,
    }
}

/// Draws a glyph about 22 points wide times `scale` around `center`, after
/// the mockup's icons.
pub fn draw_glyph(painter: &Painter, glyph: Glyph, center: Pos2, scale: f32) {
    let p = |x: f32, y: f32| center + vec2(x, y) * scale;
    let v = |x: f32, y: f32| vec2(x, y) * scale;
    let stroke = |w: f32, color: Color32| Stroke::new(w * scale, color);
    match glyph {
        Glyph::Plane => {
            painter.add(Shape::convex_polygon(vec![p(-11.0, -3.0), p(11.0, 0.0), p(-4.0, 0.0)], c([0xFF, 0xFF, 0xFF]), Stroke::NONE));
            painter.add(Shape::convex_polygon(vec![p(-4.0, 0.0), p(11.0, 0.0), p(-9.0, 4.0)], c([0xC9, 0xC1, 0xB0]), Stroke::NONE));
        }
        Glyph::Clock => {
            painter.circle_stroke(center, 8.0 * scale, stroke(1.6, c([0xC9, 0xCE, 0xD9])));
            painter.line_segment([center, p(0.0, -5.0)], stroke(1.6, c([0xC9, 0xCE, 0xD9])));
            painter.line_segment([center, p(4.0, 0.0)], stroke(1.6, c([0xC9, 0xCE, 0xD9])));
        }
        Glyph::Tree => {
            painter.rect_filled(Rect::from_min_size(p(-1.0, 1.0), v(2.0, 9.0)), CornerRadius::ZERO, c([0x6B, 0x4A, 0x32]));
            painter.circle_filled(p(0.0, -3.0), 7.0 * scale, c([0x4E, 0x8C, 0x5A]));
        }
        Glyph::Flag => {
            painter.rect_filled(Rect::from_min_size(p(-9.0, -3.0), v(14.0, 9.0)), CornerRadius::same(3), c([0x2F, 0x4A, 0x3A]));
            painter.rect_filled(Rect::from_min_size(p(5.0, -8.0), v(2.0, 9.0)), CornerRadius::ZERO, c([0xC8, 0x55, 0x3D]));
            painter.rect_filled(Rect::from_min_size(p(5.0, -8.0), v(5.0, 3.0)), CornerRadius::ZERO, c([0xC8, 0x55, 0x3D]));
        }
        Glyph::Board => {
            let r = Rect::from_center_size(center, v(18.0, 14.0));
            painter.rect(r, CornerRadius::same(2), c([0xB9, 0x8A, 0x57]), stroke(1.4, c([0x6E, 0x4A, 0x2E])), StrokeKind::Inside);
            painter.rect_filled(Rect::from_min_size(r.min + v(3.0, 3.0), v(6.0, 5.0)), CornerRadius::ZERO, c([0xF4, 0xF1, 0xEA]));
            painter.rect_filled(Rect::from_min_size(r.min + v(10.0, 5.0), v(5.0, 6.0)), CornerRadius::ZERO, c(theme::NEEDS_YOU));
        }
        Glyph::Graph => {
            let r = Rect::from_center_size(center, v(20.0, 15.0));
            painter.rect(r, CornerRadius::same(2), c([0x0F, 0x1A, 0x1C]), stroke(1.4, c([0x3A, 0x42, 0x52])), StrokeKind::Inside);
            let line = [p(-7.0, 3.0), p(-3.0, -1.0), p(0.0, 2.0), p(3.5, -4.0), p(7.0, -1.0)];
            painter.add(Shape::line(line.to_vec(), stroke(1.6, c(theme::WORKING))));
        }
    }
}
