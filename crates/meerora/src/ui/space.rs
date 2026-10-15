//! The shared space (screen 1): the scene with name tags, the sidebar with
//! all agents, the ring legend and the card for an agent that needs you.

use egui::{Align2, CornerRadius, Id, LayerId, Order, Rect, Sense, Stroke, StrokeKind, Ui, pos2, vec2};
use meerora_geom::Vec2 as GVec2;
use meerora_protocol::RingState;

use super::{ButtonStyle, RingIcon, button, fit, label, logo, open_agent, pill, portrait, ring_icon, status};
use crate::engine::Core;
use crate::theme::{self, c, ca};

pub const HEADER_H: f32 = 64.0;
pub const SIDEBAR_X: f32 = 24.0;
pub const SIDEBAR_W: f32 = 288.0;
const ROW_H: f32 = 58.0;
/// Lowest and highest camera elevation in degrees when the view is turned.
pub const MIN_ELEVATION: f32 = 12.0;
pub const MAX_ELEVATION: f32 = 80.0;
/// Degrees the view turns per point dragged.
const TURN_SPEED: f32 = 0.35;
const TILT_SPEED: f32 = 0.25;

pub fn draw(ui: &mut Ui, core: &mut Core) {
    let screen = ui.ctx().content_rect();
    let scene = ui.interact(screen, Id::new("scene"), Sense::click_and_drag());

    // Picking (the scene is the background, every panel above blocks it).
    let viewport = GVec2::new(screen.width(), screen.height());
    move_view(ui, core, &scene, viewport);
    let camera = core.scene_camera(viewport);
    let ray = scene.hover_pos().map(|p| camera.ray(GVec2::new(p.x, p.y), viewport));
    let hovered = ray.and_then(|(o, d)| core.world.pick(o, d));
    // A device (D7) in front of the agent takes the pointer. A click opens
    // its card, a drag still turns the view.
    let hovered_device = ray.and_then(|(o, d)| super::devices::pick(core, &camera, o, d, hovered));
    let hovered = hovered.filter(|_| hovered_device.is_none());
    // A tombstone nearer than both takes the pointer the same way. A click
    // opens the graveyard, a drag still turns the view.
    let hovered_stone = ray.and_then(|(o, d)| super::graveyard::pick(core, &camera, o, d, hovered, hovered_device));
    let (hovered, hovered_device) = if hovered_stone.is_some() { (None, None) } else { (hovered, hovered_device) };
    core.graveyard.hover = hovered_stone.clone();
    super::devices::after_pick(core, hovered_device, scene.clicked());
    for (i, a) in core.world.agents.iter_mut().enumerate() {
        a.hover = Some(i) == hovered;
    }
    if scene.dragged() {
        ui.ctx().set_cursor_icon(if panning(ui, &scene) { egui::CursorIcon::Move } else { egui::CursorIcon::Grabbing });
    } else if hovered.is_some() || hovered_device.is_some() || hovered_stone.is_some() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    if scene.double_clicked() && hovered.is_none() && hovered_device.is_none() && hovered_stone.is_none() {
        core.ui.view_pan = meerora_geom::Vec3::ZERO;
        core.ui.view_zoom = 1.0;
        core.ui.view_yaw = 0.0;
        core.ui.view_pitch = 0.0;
    }
    if scene.clicked()
        && let Some(i) = hovered
    {
        let id = core.world.agents[i].id.clone();
        open_agent(core, &id);
        return;
    }
    if scene.clicked()
        && let Some(id) = hovered_stone
    {
        super::graveyard::open(core, Some(id));
        return;
    }

    super::tags::draw(ui, core, &camera, viewport);
    super::graveyard::draw_hover(ui, core, &camera, viewport);
    super::devices::draw(ui, core, &camera, viewport, screen);
    draw_header(ui, core, screen);
    super::feed::header_button(ui, core, screen);
    draw_talk(ui, core, screen);
    draw_sidebar(ui, core, screen);
    super::device_list::draw_dialog(ui, core, screen);
    super::rooms::draw(ui, core, screen);
    draw_legend(ui, core, screen);
    // A capability request (D8) takes the place of the needs-you card.
    if !super::approvals::draw_space(ui, core, screen) {
        draw_notice(ui, core, screen);
    }
    if core.world.space.is_some() && core.world.agents.is_empty() {
        let painter = ui.painter();
        painter.text(screen.center(), Align2::CENTER_CENTER, "No agents in this space yet", theme::ui(16.0), c(theme::TEXT_2));
    }
    if core.world.space.is_none() {
        let painter = ui.painter();
        painter.text(
            screen.center(),
            Align2::CENTER_CENTER,
            format!("No space loaded. Expected {}", core.assets.space_path().display()),
            theme::mono(13.0),
            c(theme::TEXT_2),
        );
    }
}

/// True while the drag moves the view instead of turning it: the right or
/// middle button, or the left one with Shift held.
fn panning(ui: &Ui, scene: &egui::Response) -> bool {
    scene.dragged_by(egui::PointerButton::Secondary)
        || scene.dragged_by(egui::PointerButton::Middle)
        || (scene.dragged_by(egui::PointerButton::Primary) && ui.input(|i| i.modifiers.shift))
}

/// Dragging the background turns the camera around the space, dragging with
/// the right button (or Shift) moves it, the scroll wheel zooms towards the
/// pointer. The camera follows the pointer, so the space moves the other way.
fn move_view(ui: &mut Ui, core: &mut Core, scene: &egui::Response, viewport: GVec2) {
    let camera = core.scene_camera(viewport);
    let world_per_point = camera.view_height / viewport.y.max(1.0);
    let az = camera.azimuth_deg.to_radians();
    let el = camera.elevation_deg.to_radians();
    let right = meerora_geom::Vec3::new(az.cos(), 0.0, -az.sin());
    let up = meerora_geom::Vec3::new(-az.sin() * el.sin(), el.cos(), -az.cos() * el.sin());
    if scene.dragged() {
        let d = scene.drag_delta();
        if panning(ui, scene) {
            core.ui.view_pan += (right * d.x - up * d.y) * world_per_point;
        } else {
            core.ui.view_yaw = (core.ui.view_yaw + d.x * TURN_SPEED).rem_euclid(360.0);
            let base = core.world.camera.elevation_deg;
            core.ui.view_pitch = (base + core.ui.view_pitch - d.y * TILT_SPEED).clamp(MIN_ELEVATION, MAX_ELEVATION) - base;
        }
    }
    if let Some(pointer) = scene.hover_pos() {
        let scroll = ui.input(|i| i.smooth_scroll_delta.y);
        if scroll.abs() > 0.1 {
            let old = core.ui.view_zoom;
            let new = (old * (scroll * 0.002).exp()).clamp(0.5, 3.0);
            // Keep the point under the pointer where it is.
            let center = egui::pos2(viewport.x / 2.0 + sidebar_right() / 2.0, viewport.y / 2.0);
            let offset = (right * (pointer.x - center.x) - up * (pointer.y - center.y)) * world_per_point;
            core.ui.view_pan += offset * (1.0 - old / new);
            core.ui.view_zoom = new;
        }
    }
}

fn draw_header(ui: &mut Ui, core: &Core, screen: Rect) {
    let painter = ui.ctx().layer_painter(LayerId::new(Order::Middle, Id::new("header")));
    let bar = Rect::from_min_size(screen.min, vec2(screen.width(), HEADER_H));
    painter.rect_filled(bar, CornerRadius::ZERO, c(theme::BG));
    super::hline(&painter, bar.max.y, bar.min.x, bar.max.x);
    logo(&painter, pos2(40.0, bar.center().y), 22.0);
    painter.text(pos2(60.0, bar.center().y), Align2::LEFT_CENTER, "meerora", theme::ui_bold(22.0), c(theme::TEXT));

    let (dot, text) = if core.ui.daemon_connected {
        (theme::NEEDS_YOU, "AGENT DAEMON CONNECTED".to_string())
    } else {
        ([0xE0, 0x67, 0x5A], "AGENT DAEMON NOT RUNNING".to_string())
    };
    let galley = painter.layout_no_wrap(text, theme::mono(11.0), c(theme::TEXT_2));
    let chip =
        Rect::from_min_size(pos2(bar.max.x - 24.0 - galley.size().x - 34.0, bar.center().y - 16.0), vec2(galley.size().x + 34.0, 32.0));
    painter.rect(chip, CornerRadius::same(16), c(theme::PANEL), Stroke::new(1.0, c(theme::BORDER)), StrokeKind::Inside);
    painter.circle_filled(pos2(chip.min.x + 15.0, chip.center().y), 4.0, c(dot));
    painter.galley(pos2(chip.min.x + 26.0, chip.center().y - galley.size().y / 2.0), galley, c(theme::TEXT_2));
}

/// "Talk to any agent" (screen 1): `@name message` sends the message to that agent.
fn draw_talk(ui: &mut Ui, core: &mut Core, screen: Rect) {
    let rect = Rect::from_center_size(pos2(screen.center().x, HEADER_H / 2.0), vec2(560.0, 44.0));
    let mut send: Option<String> = None;
    egui::Area::new(Id::new("talk")).fade_in(false).fixed_pos(rect.min).order(Order::Foreground).show(ui.ctx(), |ui| {
        ui.allocate_exact_size(rect.size(), Sense::hover());
        let painter = ui.painter();
        painter.rect(rect, CornerRadius::same(22), c(theme::PANEL), Stroke::new(1.0, c(theme::BORDER)), StrokeKind::Inside);
        // The microphone of local voice sits at the right end (D16, ui/voice.rs).
        let mic_w = super::voice::talk_controls_width(core);
        let field = Rect::from_min_max(rect.min + vec2(20.0, 12.0), rect.max - vec2(20.0 + mic_w, 8.0));
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(field));
        let resp = child.add(
            egui::TextEdit::singleline(&mut core.ui.talk)
                .id(Id::new("talk-input"))
                .frame(egui::Frame::NONE)
                .hint_text(egui::RichText::new("Talk to any agent: @claude what changed in the repo?").color(c([0x6B, 0x72, 0x82])))
                .font(theme::ui(14.5))
                .desired_width(field.width()),
        );
        if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
            send = Some(core.ui.talk.trim().to_string());
        }
        if mic_w > 0.0 {
            let mic = Rect::from_min_max(pos2(rect.max.x - 8.0 - mic_w, rect.min.y), pos2(rect.max.x - 8.0, rect.max.y));
            super::voice::talk_controls(ui, core, mic, rect);
        }
    });
    let Some(text) = send.filter(|t| !t.is_empty()) else { return };
    let now = core.world.time;
    let Some(rest) = text.strip_prefix('@') else {
        core.ui.toast(now, "Start with @ and an agent's name, for example @claude", false);
        return;
    };
    let (who, message) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    let who = who.to_lowercase();
    let Some(agent) = core.world.agents.iter().find(|a| a.id == who || a.name.to_lowercase().starts_with(&who)) else {
        core.ui.toast(now, format!("No agent called {who} in this space"), true);
        return;
    };
    let (id, name) = (agent.id.clone(), agent.name.clone());
    let mock = core.agent_dir(&id).and_then(|d| d.config.runtime.as_ref()).is_none_or(|r| r.kind == meerora_protocol::RuntimeKind::Mock);
    if mock {
        core.ui.toast(now, format!("{name} is a mock agent played by the scenario"), false);
        return;
    }
    if message.trim().is_empty() {
        open_agent(core, &id);
        return;
    }
    if let Some(link) = &core.link {
        link.send(&meerora_protocol::Command::Say { agent: id, text: message.trim().to_string() });
    }
    core.ui.talk.clear();
    core.ui.toast(now, format!("Sent to {name}"), false);
}

fn draw_sidebar(ui: &mut Ui, core: &mut Core, screen: Rect) {
    let rect = Rect::from_min_max(pos2(SIDEBAR_X, HEADER_H + 20.0), pos2(SIDEBAR_X + SIDEBAR_W, screen.max.y - 24.0));
    let mut open: Option<String> = None;
    let mut hover_row: Option<usize> = None;
    let mut new_agent = false;
    let mut get_external = false;
    let mut open_graveyard = false;
    let mut pick_room = false;
    let room_name = super::rooms::current_name(ui.ctx(), core);
    // What the context menu of an agent's row asked for: open, edit or retire, and the agent.
    let mut menu: Option<(&str, String)> = None;
    // The devices of the space under the agents (screen 1, device_list.rs).
    let devices = super::device_list::rows(core);
    let mut device_click: Option<super::device_list::Click> = None;
    egui::Area::new(Id::new("sidebar")).fade_in(false).fixed_pos(rect.min).order(Order::Middle).show(ui.ctx(), |ui| {
        let (_, resp) = ui.allocate_exact_size(rect.size(), Sense::hover());
        let painter = ui.painter();
        super::panel(painter, rect, 18);
        let x = rect.min.x + 16.0;
        let inner_w = rect.width() - 32.0;
        let mut y = rect.min.y + 18.0;
        label(painter, pos2(x, y), "SPACE");
        y += 22.0;
        // The room of the space (D6). A click opens the room picker (rooms.rs).
        let card = Rect::from_min_size(pos2(x, y), vec2(inner_w, 64.0));
        let room = ui.interact(card, Id::new("room-card"), Sense::click());
        let painter = ui.painter();
        let fill = if room.hovered() { c([0x1D, 0x23, 0x2F]) } else { c([0x18, 0x1D, 0x27]) };
        painter.rect_filled(card, CornerRadius::same(12), fill);
        // A small chevron says the card opens a choice.
        let tip = pos2(card.max.x - 18.0, card.min.y + 23.0);
        let chevron = Stroke::new(1.6, c(if room.hovered() { theme::TEXT } else { theme::TEXT_2 }));
        painter.line_segment([tip + vec2(-5.0, -3.0), tip + vec2(0.0, 2.0)], chevron);
        painter.line_segment([tip + vec2(0.0, 2.0), tip + vec2(5.0, -3.0)], chevron);
        let title = fit(painter, &room_name, theme::ui_semibold(16.0), card.width() - 48.0);
        painter.text(card.min + vec2(14.0, 12.0), Align2::LEFT_TOP, title, theme::ui_semibold(16.0), c(theme::TEXT));
        if room.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        if room.clicked() {
            pick_room = true;
        }
        let stations = core.world.space.as_ref().map_or(0, |s| s.desc.stations.len());
        let online = core.world.agents.iter().filter(|a| a.online).count();
        painter.text(
            card.min + vec2(14.0, 36.0),
            Align2::LEFT_TOP,
            format!("{} agents · {online} awake · {stations} stations", core.world.agents.len()),
            theme::ui(13.0),
            c(theme::TEXT_2),
        );
        let _ = resp;
        // The agents and the devices scroll when they do not fit.
        let body = Rect::from_min_max(pos2(rect.min.x + 2.0, card.max.y + 10.0), pos2(rect.max.x - 2.0, rect.max.y - 2.0));
        let error = core.world.agents.iter().find_map(|a| a.figure_error.as_ref().map(|e| (a.name.clone(), e.clone())));
        let error = error
            .map(|err| painter.layout(format!("{} has no figure: {}", err.0, err.1), theme::mono(11.0), c([0xE0, 0x67, 0x5A]), inner_w));
        let retired = core.graveyard.agents.len();
        let content_h = 12.0
            + 22.0
            + core.world.agents.len() as f32 * (ROW_H + 6.0)
            + 96.0
            + if retired > 0 { 48.0 } else { 0.0 }
            + error.as_ref().map_or(0.0, |g| 6.0 + g.size().y)
            + 14.0
            + super::device_list::height(devices.len(), super::device_list::collapsed(core))
            + 14.0;
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(body));
        egui::ScrollArea::vertical().id_salt("sidebar-body").auto_shrink([false, false]).show(&mut child, |ui| {
            let (area, _) = ui.allocate_exact_size(vec2(body.width(), content_h.max(body.height())), Sense::hover());
            let x = area.min.x + 14.0;
            let mut y = area.min.y + 12.0;
            label(ui.painter(), pos2(x, y), "AGENTS");
            y += 22.0;
            for (i, a) in core.world.agents.iter().enumerate() {
                let row = Rect::from_min_size(pos2(x, y), vec2(inner_w, ROW_H));
                let r = ui.interact(row, Id::new(("agent-row", i)), Sense::click());
                let painter = ui.painter();
                let waiting = a.ring == RingState::NeedsYou;
                let fill = if r.hovered() || a.hover { c([0x1A, 0x1F, 0x2A]) } else { c([0x16, 0x1A, 0x23]) };
                let stroke = if waiting { Stroke::new(1.0, ca(theme::NEEDS_YOU, 150)) } else { Stroke::NONE };
                painter.rect(row, CornerRadius::same(12), fill, stroke, StrokeKind::Inside);
                portrait(painter, Rect::from_center_size(pos2(row.min.x + 26.0, row.center().y), vec2(44.0, 44.0)), core, a);
                let (st, color) = status(a);
                let pill_rect = pill(painter, pos2(row.max.x - 10.0, row.center().y), &st, color);
                let text_w = pill_rect.min.x - row.min.x - 62.0;
                let name_r = painter.text(
                    pos2(row.min.x + 54.0, row.min.y + 10.0),
                    Align2::LEFT_TOP,
                    &a.name,
                    theme::ui_semibold(15.5),
                    c(theme::TEXT),
                );
                super::index::sidebar_badge(painter, core, &a.id, name_r, pill_rect.min.x - 6.0);
                let station = a.station().map(|s| s.replace('_', " ")).unwrap_or_default();
                let sub = match (station.is_empty(), a.activity.is_empty()) {
                    (false, false) => format!("{station} · {}", a.activity),
                    (false, true) => station,
                    _ => a.activity.clone(),
                };
                let sub = super::index::sidebar_sub(core, &a.id, sub);
                let sub = fit(painter, &sub, theme::ui(12.5), text_w);
                painter.text(pos2(row.min.x + 54.0, row.min.y + 32.0), Align2::LEFT_TOP, sub, theme::ui(12.5), c(theme::TEXT_2));
                if r.hovered() {
                    hover_row = Some(i);
                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                }
                if r.clicked() {
                    open = Some(a.id.clone());
                }
                // External agents come from their manifest, so the builder does not edit them (D25).
                let editable = super::index::external(core, &a.id).is_none();
                // The helper always stays (ui/config_card.rs).
                let retirable = !super::config_card::is_helper(core, &a.id);
                r.context_menu(|ui| {
                    ui.set_min_width(190.0);
                    if ui.button("Open").clicked() {
                        menu = Some(("open", a.id.clone()));
                        ui.close();
                    }
                    if editable && ui.button("Edit").clicked() {
                        menu = Some(("edit", a.id.clone()));
                        ui.close();
                    }
                    if retirable {
                        ui.separator();
                        if ui.button(super::graveyard::MOVE).clicked() {
                            menu = Some(("retire", a.id.clone()));
                            ui.close();
                        }
                    }
                });
                y += ROW_H + 6.0;
            }
            let row = Rect::from_min_size(pos2(x, y), vec2(inner_w, 40.0));
            let r = ui.interact(row, Id::new("new-agent"), Sense::click());
            let painter = ui.painter();
            let stroke = Stroke::new(1.0, if r.hovered() { c([0x3A, 0x42, 0x52]) } else { c([0x2A, 0x30, 0x3D]) });
            for k in 0..((row.width() / 8.0) as i32) {
                let x0 = row.min.x + k as f32 * 8.0;
                painter.line_segment([pos2(x0, row.min.y), pos2(x0 + 4.0, row.min.y)], stroke);
                painter.line_segment([pos2(x0, row.max.y), pos2(x0 + 4.0, row.max.y)], stroke);
            }
            painter.text(row.center(), Align2::CENTER_CENTER, "+  New agent", theme::ui(14.0), c([0xC9, 0xCE, 0xD9]));
            if r.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
            if r.clicked() {
                new_agent = true;
            }
            y += 48.0;
            get_external = super::index::sidebar_row(ui, Rect::from_min_size(pos2(x, y), vec2(inner_w, 40.0)));
            y += 48.0;
            if retired > 0 {
                open_graveyard = super::graveyard::sidebar_row(ui, Rect::from_min_size(pos2(x, y), vec2(inner_w, 40.0)), retired);
                y += 48.0;
            }
            if let Some(galley) = error {
                y += 6.0;
                let h = galley.size().y;
                ui.painter().galley(pos2(x, y), galley, c([0xE0, 0x67, 0x5A]));
                y += h;
            }
            y += 14.0;
            device_click = super::device_list::draw(ui, core, &devices, pos2(x, y), inner_w, area.max.y - 14.0);
        });
    });
    if let Some(i) = hover_row {
        core.world.agents[i].hover = true;
    }
    match device_click {
        // A second click on the row of the open card closes it.
        Some(super::device_list::Click::Device(id)) => {
            core.devices.card = if core.devices.card.as_deref() == Some(id.as_str()) { None } else { Some(id) };
        }
        Some(super::device_list::Click::Ask) => super::device_list::open(ui.ctx(), core, ""),
        None => {}
    }
    if let Some(id) = open {
        open_agent(core, &id);
    }
    if new_agent {
        super::open_builder(core);
    }
    if get_external {
        super::index::open(core);
    }
    if pick_room {
        super::rooms::open(ui.ctx(), core);
    }
    if open_graveyard {
        super::graveyard::open(core, None);
    }
    match menu {
        Some(("open", id)) => open_agent(core, &id),
        Some(("edit", id)) => super::builder::open_edit(core, &id),
        Some((_, id)) => super::graveyard::ask_retire(core, &id),
        None => {}
    }
}

fn draw_legend(ui: &mut Ui, core: &Core, screen: Rect) {
    let painter = ui.ctx().layer_painter(LayerId::new(Order::Middle, Id::new("legend")));
    let x = screen.max.x - 236.0;
    let mut y = HEADER_H + 28.0;
    let t = core.world.time as f32;
    let rows: [(Option<RingIcon>, &str); 6] = [
        (Some(RingIcon::NeedsYou), "Full ring: needs you"),
        (Some(RingIcon::Working), "Half ring: working"),
        (Some(RingIcon::Resting), "Dim ring: resting"),
        (Some(RingIcon::Countdown), "Shrinking ring: next wake"),
        (Some(RingIcon::System), "Double ring: system access"),
        (None, "Dashed line: path walked"),
    ];
    for (icon, text) in rows {
        match icon {
            Some(icon) => ring_icon(&painter, pos2(x + 12.0, y), 11.0, icon, t),
            None => {
                for k in 0..3 {
                    let x0 = x + 1.0 + k as f32 * 8.5;
                    painter.line_segment([pos2(x0, y), pos2(x0 + 5.0, y)], Stroke::new(2.2, c(theme::SYSTEM)));
                }
            }
        }
        painter.text(pos2(x + 36.0, y), Align2::LEFT_CENTER, text, theme::ui(13.5), c([0xC9, 0xCE, 0xD9]));
        y += 25.0;
    }
}

/// Card at the bottom for the agent that most recently started waiting (D3).
fn draw_notice(ui: &mut Ui, core: &mut Core, screen: Rect) {
    let rect =
        Rect::from_min_max(pos2(SIDEBAR_X + SIDEBAR_W + 20.0, screen.max.y - 24.0 - 92.0), pos2(screen.max.x - 24.0, screen.max.y - 24.0));
    if !core.ui.daemon_connected {
        egui::Area::new(Id::new("notice")).fade_in(false).fixed_pos(rect.min).order(Order::Middle).show(ui.ctx(), |ui| {
            ui.allocate_exact_size(rect.size(), Sense::hover());
            let painter = ui.painter();
            super::panel(painter, rect, 16);
            painter.text(
                rect.min + vec2(24.0, 22.0),
                Align2::LEFT_TOP,
                "The agent daemon is not running",
                theme::ui_bold(17.0),
                c(theme::TEXT),
            );
            painter.text(
                rect.min + vec2(24.0, 50.0),
                Align2::LEFT_TOP,
                "Figures stay at home until it starts. Start it with meerora-agentd, or run cargo build once in the repository.",
                theme::ui(14.5),
                c(theme::TEXT_2),
            );
        });
        return;
    }
    let mut waiting: Vec<&crate::world::Agent> =
        core.world.agents.iter().filter(|a| a.ring == RingState::NeedsYou && !core.ui.dismissed.contains(&a.id)).collect();
    waiting.sort_by(|a, b| a.name.cmp(&b.name));
    let Some(first) = waiting.first() else { return };
    let id = first.id.clone();
    let more = waiting.len() - 1;
    let mut action: Option<bool> = None;
    egui::Area::new(Id::new("notice")).fade_in(false).fixed_pos(rect.min).order(Order::Middle).show(ui.ctx(), |ui| {
        ui.allocate_exact_size(rect.size(), Sense::hover());
        let a = core.world.agent(&id).expect("waiting agent exists");
        let painter = ui.painter();
        painter.rect(rect, CornerRadius::same(16), c(theme::PANEL), Stroke::new(1.0, ca(theme::NEEDS_YOU, 120)), StrokeKind::Inside);
        portrait(painter, Rect::from_center_size(pos2(rect.min.x + 52.0, rect.center().y), vec2(64.0, 64.0)), core, a);
        let title = if more > 0 { format!("{} needs you  ·  {more} more waiting", a.name) } else { format!("{} needs you", a.name) };
        painter.text(pos2(rect.min.x + 96.0, rect.min.y + 22.0), Align2::LEFT_TOP, title, theme::ui_bold(17.0), c(theme::TEXT));
        let text = if a.activity.is_empty() { "Waiting on the front step".to_string() } else { a.activity.clone() };
        painter.text(pos2(rect.min.x + 96.0, rect.min.y + 50.0), Align2::LEFT_TOP, text, theme::ui(15.0), c([0xC9, 0xCE, 0xD9]));
        let later = Rect::from_min_size(pos2(rect.max.x - 20.0 - 96.0, rect.center().y - 24.0), vec2(96.0, 48.0));
        let open = Rect::from_min_size(pos2(later.min.x - 12.0 - 96.0, later.min.y), vec2(96.0, 48.0));
        if button(ui, open, "Open", ButtonStyle::Primary).clicked() {
            action = Some(true);
        }
        if button(ui, later, "Later", ButtonStyle::Quiet).clicked() {
            action = Some(false);
        }
    });
    match action {
        Some(true) => open_agent(core, &id),
        Some(false) => {
            core.ui.dismissed.insert(id);
        }
        None => {}
    }
}

pub fn sidebar_right() -> f32 {
    SIDEBAR_X + SIDEBAR_W
}
