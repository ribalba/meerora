//! Capability approvals in the app (D8, Q4).
//!
//! An agent asks for a capability with `request_capability`, and its run
//! waits for the answer. The card shows who asks, what the capability allows
//! in plain words, the agent's reason and three answers: Allow once, Always
//! allow and Deny. In the space view it sits at the bottom, where the
//! needs-you card would be (screen 1). In the agent view it floats over the
//! tiles like the question panel of screen 2b. The header of the agent view
//! has an Access button that lists what the agent holds, each with Revoke.
//!
//! The daemon closes requests with `CapabilitiesChanged`. It drops the open
//! requests of that agent here, and the daemon sends the ones still open
//! right after it. The greeting sends every open request again.
//!
//! A device of the space (D7) asks the same way when the user clicks an
//! action whose capability is not approved yet. Its card says which device
//! wants which capability for which action, with the same three answers.

use std::collections::HashMap;

use egui::text::LayoutJob;
use egui::{
    Align2, Color32, CornerRadius, FontId, Galley, Id, Key, Order, Painter, Pos2, Rect, Sense, Shape, Stroke, StrokeKind, Ui, pos2, vec2,
};
use meerora_protocol::{Command, Event};

use super::{ButtonStyle, button, fit, label, portrait};
use crate::engine::Core;
use crate::theme::{self, c, ca};

/// A request the user has not answered yet.
#[derive(Clone, Debug, PartialEq)]
pub struct Request {
    pub id: String,
    pub agent: String,
    pub capability: String,
    pub reason: String,
}

#[derive(Debug, Default)]
pub struct Approvals {
    /// Open requests in the order they came.
    pub requests: Vec<Request>,
    /// What each real agent holds, as the daemon reported it. Agents without
    /// a report show the `granted` list of their agent.toml.
    pub granted: HashMap<String, Vec<String>>,
    /// The agent whose Access popover is open.
    pub popover: Option<String>,
    /// Open requests of devices of the space, in the order they came (D7, D8).
    pub devices: Vec<DeviceRequest>,
}

/// A device of the space that waits for the user's approval (D8).
#[derive(Clone, Debug, PartialEq)]
pub struct DeviceRequest {
    pub id: String,
    pub device: String,
    pub capability: String,
    /// The label of the action that waits.
    pub action: String,
}

/// Keeps the approvals in step with the daemon.
pub fn apply(state: &mut Approvals, event: &Event) {
    match event {
        // The greeting sends the open requests again.
        Event::Hello { .. } => {
            state.requests.clear();
            state.devices.clear();
        }
        Event::DeviceCapabilityRequest { id, device, capability, action } => {
            if !state.devices.iter().any(|r| r.id == *id) {
                state.devices.push(DeviceRequest {
                    id: id.clone(),
                    device: device.clone(),
                    capability: capability.clone(),
                    action: action.clone(),
                });
            }
        }
        Event::DeviceCapabilitiesChanged { device, .. } => state.devices.retain(|r| r.device != *device),
        Event::CapabilityRequest { id, agent, capability, reason } => {
            if !state.requests.iter().any(|r| r.id == *id) {
                state.requests.push(Request {
                    id: id.clone(),
                    agent: agent.clone(),
                    capability: capability.clone(),
                    reason: reason.clone(),
                });
            }
        }
        Event::CapabilitiesChanged { agent, granted } => {
            state.requests.retain(|r| r.agent != *agent);
            state.granted.insert(agent.clone(), granted.clone());
        }
        _ => {}
    }
}

/// What a capability allows, as the end of "Claude asks to ...".
pub fn describe(capability: &str) -> String {
    match capability {
        "shell" => "run commands in its own folder".into(),
        "web" => "fetch web pages and search the web".into(),
        // The app tools of D30. The terminals run as you, outside the sandbox, so apps.control says so.
        "apps.read" => "read what its windows show and take screenshots of them".into(),
        "apps.control" => "type into its terminals, which run as you, drive its browser pages and open files and apps in its view".into(),
        "apps.input" => "click and type into the real app windows on its desktop".into(),
        "apps.launch" => "start desktop apps in its sandbox on its desktop".into(),
        other => {
            let mut parts: Vec<&str> = other.split('.').collect();
            let verb = match parts.last().copied() {
                Some("write" | "set") => "change",
                Some("read" | "get") => "read",
                Some("control") => "control",
                _ => "use",
            };
            if verb != "use" {
                parts.pop();
            }
            let subject = parts.join(" ").replace('_', " ");
            if subject.is_empty() { format!("use {other}") } else { format!("{verb} the {subject}") }
        }
    }
}

fn sentence(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map(|first| first.to_uppercase().chain(chars).collect()).unwrap_or_default()
}

/// What the agent holds now.
pub fn granted_of(core: &Core, agent: &str) -> Vec<String> {
    core.ui
        .approvals
        .granted
        .get(agent)
        .cloned()
        .unwrap_or_else(|| core.agent_dir(agent).map(|d| d.config.capabilities.granted.clone()).unwrap_or_default())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Answer {
    Once,
    Always,
    Deny,
}

/// Sends the answer. The card goes away at once, the daemon confirms with `CapabilitiesChanged`.
fn decide(core: &mut Core, id: &str, answer: Answer) {
    let command = Command::DecideCapability { id: id.into(), approve: answer != Answer::Deny, remember: answer == Answer::Always };
    if let Some(link) = &core.link {
        link.send(&command);
    }
    core.ui.approvals.requests.retain(|r| r.id != id);
}

/// Open requests of agents in this space, the first one and how many more wait.
fn first_request(core: &Core, agent: Option<&str>) -> Option<(Request, usize)> {
    let mut open = core.ui.approvals.requests.iter().filter(|r| agent.is_none_or(|a| r.agent == a) && core.world.agent(&r.agent).is_some());
    let first = open.next()?.clone();
    Some((first, open.count()))
}

/// The card in the space view. Returns true when it drew one, then it takes
/// the place of the needs-you card.
pub fn draw_space(ui: &mut Ui, core: &mut Core, screen: Rect) -> bool {
    if !core.ui.daemon_connected {
        return false;
    }
    let Some((request, more)) = first_request(core, None) else { return draw_device_space(ui, core, screen) };
    let left = super::space::sidebar_right() + 20.0;
    let reason_w = text_width(screen.max.x - 24.0 - left);
    let reason = paragraph(ui.painter(), &quoted(&request.reason), theme::ui(15.0), c([0xDD, 0xE1, 0xEA]), reason_w, 2);
    let h = (24.0 + 22.0 + 6.0 + reason.size().y + 10.0 + 24.0 + 22.0).max(108.0);
    let rect = Rect::from_min_max(pos2(left, screen.max.y - 24.0 - h), pos2(screen.max.x - 24.0, screen.max.y - 24.0));
    let mut answer = None;
    egui::Area::new(Id::new("approval-space")).fade_in(false).fixed_pos(rect.min).order(Order::Middle).show(ui.ctx(), |ui| {
        ui.allocate_exact_size(rect.size(), Sense::hover());
        answer = space_card(ui, core, &request, more, rect, reason);
    });
    if let Some(answer) = answer {
        decide(core, &request.id, answer);
    }
    true
}

/// The width of the text in a space card of width `card`, next to the
/// portrait and the three buttons.
fn text_width(card: f32) -> f32 {
    card - 96.0 - (18.0 + 88.0 + 10.0 + 132.0 + 10.0 + 124.0) - 28.0
}

fn quoted(text: &str) -> String {
    format!("\u{201C}{text}\u{201D}")
}

/// The card of screen 1: who asks to do what, the reason, and the declared
/// capability with a padlock (D8) under it.
fn space_card(ui: &mut Ui, core: &Core, request: &Request, more: usize, rect: Rect, reason: std::sync::Arc<Galley>) -> Option<Answer> {
    let a = core.world.agent(&request.agent)?;
    let accent = if a.system { theme::SYSTEM } else { theme::NEEDS_YOU };
    let painter = ui.painter();
    painter.rect(rect, CornerRadius::same(18), c(theme::PANEL_2), Stroke::new(1.0, ca(accent, 150)), StrokeKind::Inside);
    portrait(painter, Rect::from_center_size(pos2(rect.min.x + 50.0, rect.center().y), vec2(64.0, 64.0)), core, a);

    let h = 48.0;
    let deny = Rect::from_min_size(pos2(rect.max.x - 18.0 - 88.0, rect.center().y - h / 2.0), vec2(88.0, h));
    let always = Rect::from_min_size(pos2(deny.min.x - 10.0 - 132.0, deny.min.y), vec2(132.0, h));
    let once = Rect::from_min_size(pos2(always.min.x - 10.0 - 124.0, deny.min.y), vec2(124.0, h));

    let x = rect.min.x + 96.0;
    let width = text_width(rect.width());
    let reason_h = reason.size().y;
    let top = rect.center().y - (22.0 + 6.0 + reason_h + 10.0 + 24.0) / 2.0;
    let title_font = theme::ui_bold(16.5);
    let title = fit(painter, &format!("{} asks to {}", a.name, describe(&request.capability)), title_font.clone(), width);
    painter.text(pos2(x, top + 11.0), Align2::LEFT_CENTER, title, title_font, c(theme::TEXT));
    painter.galley(pos2(x, top + 28.0), reason, c([0xDD, 0xE1, 0xEA]));
    let meta_y = top + 28.0 + reason_h + 10.0 + 12.0;
    let chip = lock_chip(painter, pos2(x, meta_y), &request.capability, lock_chip_width(painter, &request.capability).min(width * 0.7));
    let mut sub = Vec::new();
    if a.system {
        sub.push("system agent".to_string());
    }
    if more > 0 {
        sub.push(format!("{more} more waiting"));
    }
    if !sub.is_empty() {
        let sub = fit(painter, &sub.join(" · "), theme::ui(13.0), x + width - chip.max.x - 12.0);
        painter.text(pos2(chip.max.x + 12.0, meta_y), Align2::LEFT_CENTER, sub, theme::ui(13.0), c(theme::TEXT_2));
    }

    let mut answer = None;
    if button(ui, once, "Allow once", ButtonStyle::Primary).clicked() {
        answer = Some(Answer::Once);
    }
    if button(ui, always, "Always allow", ButtonStyle::Quiet).clicked() {
        answer = Some(Answer::Always);
    }
    if button(ui, deny, "Deny", ButtonStyle::Quiet).clicked() {
        answer = Some(Answer::Deny);
    }
    answer
}

/// The floating card in the agent view of `id`, above the chat input.
pub fn draw_agent(ui: &mut Ui, core: &mut Core, id: &str, content: Rect) {
    if !core.ui.daemon_connected {
        return;
    }
    let Some((request, more)) = first_request(core, Some(id)) else { return };
    let Some(a) = core.world.agent(id) else { return };
    let w = 384.0_f32.min(content.width() - 32.0);
    let inner = w - 28.0;
    let painter = ui.painter();
    let what = paragraph(painter, &format!("{}.", sentence(&describe(&request.capability))), theme::ui(14.5), c(theme::TEXT), inner, 2);
    let why = paragraph(painter, &quoted(&request.reason), theme::ui(14.0), c([0xC9, 0xCE, 0xD9]), inner, 3);
    let h = 14.0 + 42.0 + 12.0 + what.size().y + 4.0 + why.size().y + 14.0 + 40.0 + 14.0;
    let rect = Rect::from_min_size(pos2(content.max.x - 16.0 - w, content.max.y - 92.0 - h), vec2(w, h));
    let (name, system) = (a.name.clone(), a.system);
    let mut answer = None;
    egui::Area::new(Id::new("approval-agent")).fade_in(false).fixed_pos(rect.min).order(Order::Foreground).show(ui.ctx(), |ui| {
        ui.allocate_exact_size(rect.size(), Sense::hover());
        let Some(a) = core.world.agent(id) else { return };
        let painter = ui.painter();
        let shadow = egui::epaint::Shadow { offset: [0, 18], blur: 44, spread: 0, color: Color32::from_black_alpha(130) };
        painter.add(shadow.as_shape(rect, CornerRadius::same(16)));
        let accent = if system { theme::SYSTEM } else { theme::NEEDS_YOU };
        painter.rect(rect, CornerRadius::same(16), c(theme::PANEL_2), Stroke::new(1.0, c(accent)), StrokeKind::Inside);
        let face = Rect::from_min_size(rect.min + vec2(14.0, 14.0), vec2(42.0, 42.0));
        painter.rect_filled(face, CornerRadius::same(10), c([0x1A, 0x1F, 0x2A]));
        portrait(painter, face, core, a);
        let x = face.max.x + 12.0;
        let title = if more > 0 { format!("{name}  ·  {more} more waiting") } else { name.clone() };
        painter.text(
            pos2(x, face.min.y + 2.0),
            Align2::LEFT_TOP,
            fit(painter, &title, theme::ui_bold(15.0), rect.max.x - x - 14.0),
            theme::ui_bold(15.0),
            c(theme::TEXT),
        );
        let asks = fit(painter, &format!("ASKS FOR {}", request.capability.to_uppercase()), theme::mono(10.5), rect.max.x - x - 14.0);
        painter.text(pos2(x, face.min.y + 25.0), Align2::LEFT_TOP, asks, theme::mono(10.5), c(accent));
        let mut y = face.max.y + 12.0;
        let what_h = what.size().y;
        painter.galley(pos2(rect.min.x + 14.0, y), what, c(theme::TEXT));
        y += what_h + 4.0;
        let why_h = why.size().y;
        painter.galley(pos2(rect.min.x + 14.0, y), why, c([0xC9, 0xCE, 0xD9]));
        y += why_h + 14.0;
        let (deny_w, always_w, gap) = (72.0, 120.0, 6.0);
        let deny = Rect::from_min_size(pos2(rect.max.x - 14.0 - deny_w, y), vec2(deny_w, 40.0));
        let always = Rect::from_min_size(pos2(deny.min.x - gap - always_w, y), vec2(always_w, 40.0));
        let once = Rect::from_min_max(pos2(rect.min.x + 14.0, y), pos2(always.min.x - gap, y + 40.0));
        if button(ui, once, "Allow once", ButtonStyle::Primary).clicked() {
            answer = Some(Answer::Once);
        }
        if button(ui, always, "Always allow", ButtonStyle::Quiet).clicked() {
            answer = Some(Answer::Always);
        }
        if button(ui, deny, "Deny", ButtonStyle::Quiet).clicked() {
            answer = Some(Answer::Deny);
        }
    });
    if let Some(answer) = answer {
        decide(core, &request.id, answer);
    }
}

/// The Access button of the agent header and the popover with the granted
/// capabilities. It sits left of `right` when the app tabs, which span
/// `tabs`, leave room there, else left of the tabs.
pub fn access_button(ui: &mut Ui, core: &mut Core, id: &str, tabs: (f32, f32), right: f32, center_y: f32) {
    let granted = granted_of(core, id);
    let right = if right - 40.0 >= tabs.1 + 8.0 { right } else { tabs.0 - 8.0 };
    let rect = Rect::from_min_size(pos2(right - 40.0, center_y - 19.0), vec2(40.0, 38.0));
    let open = core.ui.approvals.popover.as_deref() == Some(id);
    let resp = ui.interact(rect, Id::new(("access", id)), Sense::click());
    let painter = ui.painter();
    let fill = if resp.hovered() || open { c([0x1A, 0x1F, 0x2A]) } else { Color32::TRANSPARENT };
    let border = if resp.hovered() || open { c([0x3A, 0x42, 0x52]) } else { c([0x2A, 0x30, 0x3D]) };
    painter.rect(rect, CornerRadius::same(10), fill, Stroke::new(1.0, border), StrokeKind::Inside);
    let tint = if granted.is_empty() { c(theme::TEXT_2) } else { c(theme::SYSTEM_2) };
    lock(painter, rect.center() - vec2(if granted.is_empty() { 0.0 } else { 4.0 }, 0.0), 15.0, tint);
    if !granted.is_empty() {
        painter.text(
            pos2(rect.max.x - 9.0, rect.center().y + 1.0),
            Align2::CENTER_CENTER,
            granted.len().to_string(),
            theme::mono(10.5),
            tint,
        );
    }
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let resp = resp.on_hover_text("What this agent may do");
    if resp.clicked() {
        core.ui.approvals.popover = if open { None } else { Some(id.to_string()) };
    }
    if core.ui.approvals.popover.as_deref() == Some(id) {
        popover(ui, core, id, rect, &granted);
    }
}

fn popover(ui: &mut Ui, core: &mut Core, id: &str, anchor: Rect, granted: &[String]) {
    let w = 384.0;
    let row_h = 52.0;
    let body_h = if granted.is_empty() { 44.0 } else { granted.len() as f32 * row_h };
    let h = 16.0 + 20.0 + body_h + 34.0;
    let screen = ui.ctx().content_rect();
    let x = (anchor.max.x - w).max(screen.min.x + 100.0).min(screen.max.x - 8.0 - w);
    let rect = Rect::from_min_size(pos2(x, anchor.max.y + 8.0), vec2(w, h));
    let name = core.world.agent(id).map_or(id.to_string(), |a| a.name.clone());
    let mut revoke = None;
    egui::Area::new(Id::new("access-popover")).fade_in(false).fixed_pos(rect.min).order(Order::Foreground).show(ui.ctx(), |ui| {
        ui.allocate_exact_size(rect.size(), Sense::hover());
        let painter = ui.painter();
        let shadow = egui::epaint::Shadow { offset: [0, 14], blur: 36, spread: 0, color: Color32::from_black_alpha(130) };
        painter.add(shadow.as_shape(rect, CornerRadius::same(14)));
        painter.rect(rect, CornerRadius::same(14), c(theme::PANEL_2), Stroke::new(1.0, c([0x2E, 0x36, 0x46])), StrokeKind::Inside);
        let x = rect.min.x + 16.0;
        let mut y = rect.min.y + 14.0;
        label(painter, pos2(x, y), "ACCESS");
        y += 22.0;
        if granted.is_empty() {
            let text = format!("Nothing granted. {name} reads and edits its own folder and asks when a task needs more.");
            let galley = paragraph(painter, &text, theme::ui(13.5), c(theme::TEXT_2), w - 32.0, 2);
            painter.galley(pos2(x, y + 2.0), galley, c(theme::TEXT_2));
            y += body_h;
        }
        for capability in granted {
            let row = Rect::from_min_size(pos2(x, y), vec2(w - 32.0, row_h - 6.0));
            let painter = ui.painter();
            painter.rect_filled(row, CornerRadius::same(10), c([0x1B, 0x20, 0x30]));
            lock(painter, pos2(row.min.x + 16.0, row.min.y + 15.0), 12.0, c(theme::SYSTEM_2));
            let button_r = Rect::from_min_size(pos2(row.max.x - 8.0 - 74.0, row.center().y - 15.0), vec2(74.0, 30.0));
            let text_w = button_r.min.x - row.min.x - 38.0;
            let mono = fit(painter, capability, theme::mono(12.0), text_w);
            painter.text(pos2(row.min.x + 30.0, row.min.y + 8.0), Align2::LEFT_TOP, mono, theme::mono(12.0), c(theme::TEXT));
            let plain = fit(painter, &sentence(&describe(capability)), theme::ui(12.5), text_w);
            painter.text(pos2(row.min.x + 30.0, row.min.y + 25.0), Align2::LEFT_TOP, plain, theme::ui(12.5), c(theme::TEXT_2));
            if button(ui, button_r, "Revoke", ButtonStyle::Quiet).clicked() {
                revoke = Some(capability.clone());
            }
            y += row_h;
        }
        let painter = ui.painter();
        painter.text(
            pos2(x, rect.max.y - 22.0),
            Align2::LEFT_CENTER,
            "Revoking stops a run that uses it.",
            theme::ui(12.0),
            c(theme::TEXT_2),
        );
    });
    if let Some(capability) = revoke {
        if let Some(link) = &core.link {
            link.send(&Command::RevokeCapability { agent: id.into(), capability: capability.clone() });
        }
        let list = core.ui.approvals.granted.entry(id.to_string()).or_insert_with(|| granted.to_vec());
        list.retain(|g| *g != capability);
    }
    // A click elsewhere or Esc closes it. Esc must not also leave the agent view.
    let clicked_outside =
        ui.input(|i| i.pointer.any_click() && i.pointer.interact_pos().is_some_and(|p| !rect.contains(p) && !anchor.contains(p)));
    let escape = ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Escape));
    if clicked_outside || escape {
        core.ui.approvals.popover = None;
    }
}

/// A wrapped paragraph of at most `rows` rows, cut with an ellipsis.
fn paragraph(painter: &Painter, text: &str, font: FontId, color: Color32, width: f32, rows: usize) -> std::sync::Arc<Galley> {
    let mut job = LayoutJob::simple(text.to_string(), font, color, width);
    job.wrap.max_rows = rows;
    job.wrap.break_anywhere = false;
    job.wrap.overflow_character = Some('…');
    painter.layout_job(job)
}

/// A padlock, as on the device card of screen 1.
pub(super) fn lock(painter: &Painter, center: Pos2, size: f32, color: Color32) {
    let stroke = Stroke::new(1.6, color);
    let body = Rect::from_center_size(center + vec2(0.0, size * 0.18), vec2(size * 0.78, size * 0.56));
    painter.rect_stroke(body, CornerRadius::same(2), stroke, StrokeKind::Middle);
    let r = size * 0.24;
    let top = body.min.y;
    let n = 12;
    let arc: Vec<Pos2> = (0..=n)
        .map(|k| std::f32::consts::PI * k as f32 / n as f32)
        .map(|t| pos2(center.x - r * t.cos(), top - r * 1.15 * t.sin()))
        .collect();
    painter.add(Shape::line(arc, stroke));
}

fn lock_chip_width(painter: &Painter, capability: &str) -> f32 {
    painter.layout_no_wrap(capability.to_string(), theme::mono(10.5), c(theme::TEXT)).size().x + 34.0
}

/// The declared capability as a chip with a padlock (D8). `left` is the
/// middle of its left edge.
fn lock_chip(painter: &Painter, left: Pos2, capability: &str, width: f32) -> Rect {
    let rect = Rect::from_min_size(pos2(left.x, left.y - 12.0), vec2(width, 24.0));
    painter.rect_filled(rect, CornerRadius::same(8), c([0x1B, 0x20, 0x30]));
    lock(painter, pos2(rect.min.x + 13.0, rect.center().y), 11.0, c(theme::SYSTEM_2));
    let text = fit(painter, capability, theme::mono(10.5), width - 32.0);
    painter.text(pos2(rect.min.x + 24.0, rect.center().y), Align2::LEFT_CENTER, text, theme::mono(10.5), c([0xC9, 0xCE, 0xD9]));
    rect
}

// Devices (D7, D8).

/// The card for the first open request of a device of the space, at the
/// bottom of the space view like an agent's request. Returns true when it drew one.
fn draw_device_space(ui: &mut Ui, core: &mut Core, screen: Rect) -> bool {
    let Some(request) = core.ui.approvals.devices.first().cloned() else { return false };
    let more = core.ui.approvals.devices.len() - 1;
    let name = core.devices.device(&request.device).map_or(request.device.clone(), |d| d.desc.name.clone());
    let glyph = core.devices.device(&request.device).map(super::devices::glyph_for);
    let left = super::space::sidebar_right() + 20.0;
    let width = text_width(screen.max.x - 24.0 - left);
    let what = format!(
        "\u{201C}{}\u{201D} needs your OK once. Always allow keeps it for this device, which is part of your space.",
        request.action
    );
    let what = paragraph(ui.painter(), &what, theme::ui(15.0), c([0xDD, 0xE1, 0xEA]), width, 2);
    let h = (24.0 + 22.0 + 6.0 + what.size().y + 10.0 + 24.0 + 22.0).max(108.0);
    let rect = Rect::from_min_max(pos2(left, screen.max.y - 24.0 - h), pos2(screen.max.x - 24.0, screen.max.y - 24.0));
    let mut answer = None;
    egui::Area::new(Id::new("approval-device")).fade_in(false).fixed_pos(rect.min).order(Order::Middle).show(ui.ctx(), |ui| {
        ui.allocate_exact_size(rect.size(), Sense::hover());
        let painter = ui.painter();
        painter.rect(rect, CornerRadius::same(18), c(theme::PANEL_2), Stroke::new(1.0, ca(theme::NEEDS_YOU, 150)), StrokeKind::Inside);
        let face = Rect::from_center_size(pos2(rect.min.x + 50.0, rect.center().y), vec2(64.0, 64.0));
        painter.rect_filled(face, CornerRadius::same(14), c([0x1A, 0x1F, 0x2A]));
        match glyph {
            Some(glyph) => super::devices::draw_glyph(painter, glyph, face.center(), 1.8),
            None => lock(painter, face.center(), 22.0, c(theme::SYSTEM_2)),
        }
        let h = 48.0;
        let deny = Rect::from_min_size(pos2(rect.max.x - 18.0 - 88.0, rect.center().y - h / 2.0), vec2(88.0, h));
        let always = Rect::from_min_size(pos2(deny.min.x - 10.0 - 132.0, deny.min.y), vec2(132.0, h));
        let once = Rect::from_min_size(pos2(always.min.x - 10.0 - 124.0, deny.min.y), vec2(124.0, h));
        let x = rect.min.x + 96.0;
        let what_h = what.size().y;
        let top = rect.center().y - (22.0 + 6.0 + what_h + 10.0 + 24.0) / 2.0;
        let label = request.device.replace(['_', '-'], " ");
        let title = fit(painter, &format!("The {label} device wants {}", request.capability), theme::ui_bold(16.5), width);
        painter.text(pos2(x, top + 11.0), Align2::LEFT_CENTER, title, theme::ui_bold(16.5), c(theme::TEXT));
        painter.galley(pos2(x, top + 28.0), what, c([0xDD, 0xE1, 0xEA]));
        let meta_y = top + 28.0 + what_h + 10.0 + 12.0;
        let chip = lock_chip(painter, pos2(x, meta_y), &request.capability, lock_chip_width(painter, &request.capability).min(width * 0.7));
        let mut sub = vec![name.clone()];
        if more > 0 {
            sub.push(format!("{more} more waiting"));
        }
        let sub = fit(painter, &sub.join(" · "), theme::ui(13.0), x + width - chip.max.x - 12.0);
        painter.text(pos2(chip.max.x + 12.0, meta_y), Align2::LEFT_CENTER, sub, theme::ui(13.0), c(theme::TEXT_2));
        if button(ui, once, "Allow once", ButtonStyle::Primary).clicked() {
            answer = Some(Answer::Once);
        }
        if button(ui, always, "Always allow", ButtonStyle::Quiet).clicked() {
            answer = Some(Answer::Always);
        }
        if button(ui, deny, "Deny", ButtonStyle::Quiet).clicked() {
            answer = Some(Answer::Deny);
        }
    });
    if let Some(answer) = answer {
        let command =
            Command::DecideCapability { id: request.id.clone(), approve: answer != Answer::Deny, remember: answer == Answer::Always };
        if let Some(link) = &core.link {
            link.send(&command);
        }
        core.ui.approvals.devices.retain(|r| r.id != request.id);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(id: &str, agent: &str) -> Event {
        Event::CapabilityRequest { id: id.into(), agent: agent.into(), capability: "web".into(), reason: "docs".into() }
    }

    #[test]
    fn requests_follow_the_daemon() {
        let mut state = Approvals::default();
        apply(&mut state, &request("cap-1", "claude"));
        apply(&mut state, &request("cap-1", "claude"));
        apply(&mut state, &request("cap-2", "wren"));
        assert_eq!(state.requests.len(), 2, "a request sent again is kept once");
        apply(&mut state, &Event::CapabilitiesChanged { agent: "claude".into(), granted: vec!["web".into()] });
        assert_eq!(state.requests.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(), ["cap-2"]);
        assert_eq!(state.granted["claude"], ["web"]);
        apply(&mut state, &Event::Hello { version: 3, daemon: "d".into() });
        assert!(state.requests.is_empty(), "the greeting sends the open requests again");
        assert_eq!(state.granted["claude"], ["web"]);
    }

    #[test]
    fn device_requests_follow_the_daemon() {
        let mut state = Approvals::default();
        let ask = Event::DeviceCapabilityRequest {
            id: "cap-3".into(),
            device: "airplane".into(),
            capability: "network.airplane_mode.write".into(),
            action: "Toggle airplane mode".into(),
        };
        apply(&mut state, &ask);
        apply(&mut state, &ask);
        assert_eq!(state.devices.len(), 1);
        apply(&mut state, &Event::DeviceCapabilitiesChanged { device: "airplane".into(), granted: vec![] });
        assert!(state.devices.is_empty(), "a decision closes the card");
        apply(&mut state, &ask);
        apply(&mut state, &Event::Hello { version: 3, daemon: "d".into() });
        assert!(state.devices.is_empty());
    }

    #[test]
    fn capabilities_in_plain_words() {
        assert_eq!(describe("web"), "fetch web pages and search the web");
        assert_eq!(describe("shell"), "run commands in its own folder");
        assert_eq!(describe("apps.launch"), "start desktop apps in its sandbox on its desktop");
        assert_eq!(describe("apps.read"), "read what its windows show and take screenshots of them");
        assert!(describe("apps.control").contains("run as you"), "the terminals are not sandboxed");
        assert_eq!(describe("apps.input"), "click and type into the real app windows on its desktop");
        assert_eq!(describe("network.airplane_mode.write"), "change the network airplane mode");
        assert_eq!(describe("mail.read"), "read the mail");
        assert_eq!(describe("camera"), "use the camera");
        assert_eq!(sentence("fetch web pages"), "Fetch web pages");
    }
}
