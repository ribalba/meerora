//! Real apps as tiles of the agent view (D11, D15 phase 2). A window of an
//! agent's desktop opens as a tile in that agent's layout, shows the
//! window's title on its head and draws the window's image. A click into
//! it gives the window the keyboard, and from then on every key goes to it
//! (app.rs routes them), Esc included. A click anywhere else, or Alt+Tab,
//! takes the keyboard back. The cross on the head asks the window to close,
//! and the tile goes when the window is gone.
//!
//! "Your apps" in the launcher lists the installed apps and starts them on
//! the agent's desktop.
//!
//! A window shows the pointer its app asks for: a named shape as the egui
//! cursor, or the app's own cursor image drawn over the tile. Windows an
//! agent started in its sandbox carry the agent's name on their head (D12).

use egui::{Align2, Color32, CornerRadius, Event, Id, Rect, Sense, Ui, pos2, vec2};

use super::{AppTab, fit, pill};
use crate::engine::Core;
use crate::theme::{self, c};

/// What the window tiles need before the views draw: the output size, the
/// compositor's events turned into tiles, the clipboard and the toasts.
pub fn begin(ui: &mut Ui, core: &mut Core) {
    core.desktops.begin_frame();
    let screen = ui.ctx().content_rect();
    let content = super::agent::content_rect(screen);
    core.desktops.set_output(content.width(), content.height(), ui.ctx().pixels_per_point());
    for (text, error) in core.desktops.take_toasts() {
        core.ui.toast(core.world.time, text, error);
    }
    let mut changed = false;
    for event in core.desktops.take_events() {
        match event {
            meerora_compositor::Event::Opened(id) => changed |= opened(core, id, content),
            meerora_compositor::Event::Closed(id) => {
                core.desktops.forget(id);
                changed |= close_tiles(core, id);
            }
            meerora_compositor::Event::Changed(_) => {}
            meerora_compositor::Event::Selection(text) => ui.ctx().copy_text(text),
            // egui has no primary selection of the host, so middle-click paste stays among the apps.
            meerora_compositor::Event::PrimarySelection(_) => {}
        }
    }
    changed |= prune(core);
    if changed {
        super::tiles::save_layouts(core);
    }
}

/// After every view drew, the keyboard leaves a window on a click elsewhere.
pub fn end(ui: &mut Ui, core: &mut Core) {
    let pressed = ui.input(|i| i.pointer.any_pressed());
    core.desktops.end_frame(pressed);
}

/// A new window opens as a tile of its agent and becomes the selected tab
/// when that agent's view shows.
fn opened(core: &mut Core, id: u32, content: Rect) -> bool {
    let Some(info) = core.desktops.window(id) else { return false };
    let agent = info.desktop;
    if core.world.agent(&agent).is_none() {
        return false;
    }
    let apps = super::agent::apps_of(core, &agent);
    super::tiles::layout_for(core, &agent, &apps, content).open(AppTab::Window(id), content);
    if core.ui.view == super::View::Agent(agent) {
        core.ui.tab = AppTab::Window(id);
    }
    true
}

/// Takes the tile of a closed window out of every layout.
fn close_tiles(core: &mut Core, id: u32) -> bool {
    let mut changed = false;
    for layout in core.ui.tiles.iter_mut().flat_map(|l| l.values_mut()) {
        changed |= layout.close(AppTab::Window(id));
    }
    if core.ui.tab == AppTab::Window(id) {
        core.ui.tab = AppTab::Chat;
    }
    changed
}

/// Takes out tiles of windows that do not exist or belong to another
/// agent, for example ones loaded from an old `layouts.json`.
fn prune(core: &mut Core) -> bool {
    let Some(layouts) = core.ui.tiles.as_mut() else { return false };
    let windows = core.desktops.windows();
    let mut changed = false;
    for (agent, layout) in layouts.iter_mut() {
        let stale: Vec<AppTab> = layout
            .apps()
            .into_iter()
            .filter(|t| matches!(t, AppTab::Window(id) if !windows.iter().any(|w| w.id == *id && w.desktop == *agent)))
            .collect();
        for tab in stale {
            changed |= layout.close(tab);
        }
    }
    changed
}

/// The name of a window's tab.
pub fn title(core: &Core, id: u32) -> String {
    core.desktops.title(id)
}

/// Draws window `id` in its tile and routes the pointer to it. Returns true
/// while the window has the keyboard.
pub fn tile(ui: &mut Ui, core: &mut Core, id: u32, head: Rect, body: Rect) -> bool {
    let tile = Rect::from_min_max(head.min, body.max);
    let info = core.desktops.window(id);
    let app_id = info.as_ref().map(|w| w.app_id.clone()).unwrap_or_default();
    let focused = core.desktops.focused() == Some(id);
    // D12: who controls the window. An app an agent started runs in its sandbox.
    let owner = info.as_ref().filter(|w| w.sandboxed).map(|w| {
        let agent = core.world.agent(&w.desktop);
        (agent.map_or_else(|| w.desktop.clone(), |a| a.name.clone()), agent.map_or(theme::WORKING, |a| a.color))
    });
    // The agent acts on windows of its desktop through the app tools (D30),
    // and Take over stops it (D12).
    let desktop = info.as_ref().map(|w| w.desktop.clone()).unwrap_or_default();
    let agent = core.world.agent(&desktop).map(|a| (a.name.clone(), a.color));
    let doing = crate::agent_apps::doing(core, &desktop, AppTab::Window(id));
    let taken = core.agent_apps.is_taken(&desktop, AppTab::Window(id));
    let recent = core.agent_apps.last_act(&desktop, AppTab::Window(id)).is_some_and(|(_, at)| core.world.time - at < 30.0);
    let chip = match (&owner, focused, doing) {
        _ if taken => ("YOU · TAKEN OVER".to_string(), theme::NEEDS_YOU),
        (_, _, Some(verb)) => {
            let name = agent.as_ref().map_or(desktop.as_str(), |(n, _)| n.as_str());
            (format!("{} IS {}", name.to_uppercase(), verb.to_uppercase()), theme::WORKING)
        }
        (_, true, None) => ("YOU · TYPING".to_string(), theme::NEEDS_YOU),
        (Some((name, color)), false, None) => (format!("{} · SANDBOX", name.to_uppercase()), *color),
        (None, false, None) => ("YOU".to_string(), [0xB9, 0xBF, 0xCC]),
    };
    let mut right = head.max.x - 12.0;
    if taken || recent || owner.is_some() {
        let b = Rect::from_min_size(pos2(right - 92.0, head.center().y - 13.0), vec2(92.0, 26.0));
        let text = if taken { "Hand back" } else { "Take over" };
        if super::button(ui, b, text, super::ButtonStyle::Quiet).clicked() {
            core.agent_apps.set_taken(&desktop, AppTab::Window(id), !taken);
        }
        right = b.min.x - 10.0;
    }
    let chip_r = pill(ui.painter(), pos2(right, head.center().y), &chip.0, chip.1);
    if !app_id.is_empty() {
        let painter = ui.painter();
        let room = chip_r.min.x - head.center().x - 12.0;
        if room > 40.0 {
            let text = fit(painter, &app_id, theme::mono(10.5), room);
            painter.text(pos2(chip_r.min.x - 10.0, head.center().y), Align2::RIGHT_CENTER, text, theme::mono(10.5), c(theme::TEXT_2));
        }
    }

    // The window fills its tile, debounced while a border is dragged.
    let dragging = ui.input(|i| i.pointer.primary_down());
    core.desktops.fit(id, (body.width().floor() as u32, body.height().floor() as u32), dragging);

    let corners = CornerRadius { nw: 0, ne: 0, sw: 13, se: 13 };
    let painter = ui.painter_at(body);
    painter.rect_filled(body, corners, c(theme::TERMINAL_BG));
    let Some((texture, size)) = core.desktops.texture(ui.ctx(), id) else {
        super::agent::placeholder(ui, body, "Waiting for the window", "It shows here as soon as the app draws its first frame.");
        core.desktops.drawn(id);
        return focused;
    };
    let image = Rect::from_min_size(body.min, size);
    let round = if (image.max - body.max).length() < 2.0 { corners } else { CornerRadius::ZERO };
    let uv = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
    painter.add(egui::epaint::RectShape::filled(image, round, Color32::WHITE).with_texture(texture, uv));
    core.desktops.drawn(id);

    // Pointer and wheel go to the window under the pointer, and to the
    // window a button went down on until it is up again.
    let response = ui.interact(body, Id::new(("window", id)), Sense::click_and_drag());
    let hovered = response.hovered() || response.dragged();
    let held = core.desktops.pressed_on() == Some(id);
    if hovered {
        match core.desktops.cursor(ui.ctx()) {
            crate::desktop::TileCursor::Icon(icon) => ui.ctx().set_cursor_icon(icon),
            crate::desktop::TileCursor::Image { texture, size, hotspot } => {
                ui.ctx().set_cursor_icon(egui::CursorIcon::None);
                if let Some(pos) = ui.ctx().pointer_hover_pos() {
                    let layer = egui::LayerId::new(egui::Order::Tooltip, Id::new(("window-cursor", id)));
                    let uv = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
                    ui.ctx().layer_painter(layer).image(texture, Rect::from_min_size(pos - hotspot, size), uv, Color32::WHITE);
                }
            }
        }
    }
    let events = ui.input(|i| i.events.clone());
    let local = |p: egui::Pos2| p - image.min;
    for event in events {
        match event {
            Event::PointerMoved(p) if hovered || held => {
                let v = local(p);
                core.desktops.pointer_motion(id, v.x, v.y);
            }
            Event::PointerButton { pos, button, pressed, .. } => {
                let inside = body.contains(pos) && hovered;
                if pressed && tile.contains(pos) && ui.rect_contains_pointer(tile) {
                    // A press anywhere on the tile keeps or gives the window the keyboard.
                    // egui's focus leaves its text fields too, so it switches IME off
                    // and winit keeps sending plain key events.
                    core.desktops.claim_press();
                    if !focused {
                        core.desktops.focus(Some(id));
                    }
                    ui.memory_mut(|m| m.request_focus(response.id));
                    core.ui.tab = AppTab::Window(id);
                }
                let Some(code) = button_code(button) else { continue };
                if (pressed && inside) || (!pressed && core.desktops.pressed_on() == Some(id)) {
                    let v = local(pos);
                    core.desktops.pointer_motion(id, v.x, v.y);
                    core.desktops.pointer_button(id, code, pressed);
                }
            }
            Event::MouseWheel { unit, delta, .. } if hovered => {
                // egui's delta moves the content and Wayland's axis scrolls the view, so the signs flip.
                match unit {
                    egui::MouseWheelUnit::Line => core.desktops.scroll_clicks(id, f64::from(-delta.x), f64::from(-delta.y)),
                    egui::MouseWheelUnit::Point => core.desktops.scroll(id, f64::from(-delta.x), f64::from(-delta.y)),
                    egui::MouseWheelUnit::Page => core.desktops.scroll(id, 0.0, f64::from(-delta.y * body.height())),
                }
            }
            _ => {}
        }
    }
    // The agent's cursor where it clicked last, for a few seconds (D12).
    if let (Some((x, y, at)), Some((name, color))) = (core.agent_apps.last_click(&desktop, id), &agent)
        && core.world.time - at < 4.0
    {
        agent_cursor(&ui.painter_at(body), image.min + vec2(x, y), name, *color);
    }
    if focused {
        // A thin frame in the signal colour while the window has the keyboard.
        let frame = tile.shrink(0.5);
        ui.painter().rect_stroke(
            frame,
            CornerRadius::same(14),
            egui::Stroke::new(1.0, theme::ca(theme::NEEDS_YOU, 120)),
            egui::StrokeKind::Inside,
        );
    }
    core.desktops.focused() == Some(id)
}

/// The agent's labelled cursor at `at`: an arrow in its colour and its name.
fn agent_cursor(painter: &egui::Painter, at: egui::Pos2, name: &str, color: [u8; 3]) {
    let arrow = vec![at, at + vec2(0.0, 16.0), at + vec2(11.0, 12.0)];
    painter.add(egui::Shape::convex_polygon(arrow, c(color), egui::Stroke::new(1.2, Color32::from_black_alpha(200))));
    let font = theme::mono(10.5);
    let galley = painter.layout_no_wrap(name.to_uppercase(), font, c(theme::BG));
    let label = Rect::from_min_size(at + vec2(10.0, 16.0), galley.size() + vec2(10.0, 4.0));
    painter.rect_filled(label, CornerRadius::same(5), c(color));
    painter.galley(label.min + vec2(5.0, 2.0), galley, c(theme::BG));
}

fn button_code(button: egui::PointerButton) -> Option<u32> {
    use meerora_compositor::{BTN_EXTRA, BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, BTN_SIDE};
    Some(match button {
        egui::PointerButton::Primary => BTN_LEFT,
        egui::PointerButton::Secondary => BTN_RIGHT,
        egui::PointerButton::Middle => BTN_MIDDLE,
        egui::PointerButton::Extra1 => BTN_SIDE,
        egui::PointerButton::Extra2 => BTN_EXTRA,
    })
}

/// Asks window `id` to close. Its tile goes once the window is gone.
pub fn request_close(core: &mut Core, id: u32) {
    if let Some(c) = core.desktops.compositor() {
        c.close(id);
    }
}

/// Height of "Your apps" in the launcher for `rows` visible rows.
pub const YOUR_APPS_HEAD: f32 = 74.0;
pub const APP_ROW_H: f32 = 34.0;

/// "Your apps" in the launcher, inside `rect`: a search field and the
/// installed apps, most recently used first. Returns true when an app
/// started, so the launcher closes.
pub fn your_apps(ui: &mut Ui, core: &mut Core, agent: &str, rect: Rect) -> bool {
    let painter = ui.painter();
    super::hline(painter, rect.min.y, rect.min.x + 8.0, rect.max.x - 8.0);
    painter.text(rect.min + vec2(16.0, 18.0), Align2::LEFT_CENTER, "YOUR APPS", theme::mono(10.5), c(theme::TEXT_2));
    painter.text(
        pos2(rect.max.x - 16.0, rect.min.y + 18.0),
        Align2::RIGHT_CENTER,
        "run as you, on this agent's desktop",
        theme::ui(11.5),
        c([0x6B, 0x72, 0x82]),
    );
    let field = Rect::from_min_size(rect.min + vec2(12.0, 32.0), vec2(rect.width() - 24.0, 32.0));
    ui.painter().rect(field, CornerRadius::same(8), c(theme::BG), egui::Stroke::new(1.0, c(theme::BORDER)), egui::StrokeKind::Inside);
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(field.shrink2(vec2(10.0, 7.0))));
    let edit = egui::TextEdit::singleline(&mut core.desktops.search)
        .id(Id::new(("your-apps-search", agent)))
        .font(theme::ui(13.5))
        .frame(egui::Frame::NONE)
        .margin(egui::Margin::ZERO)
        .hint_text(egui::RichText::new("Search your apps").color(c([0x6B, 0x72, 0x82])))
        .desired_width(field.width() - 20.0);
    let search = child.add(edit);
    let enter = search.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));

    let query = core.desktops.search.clone();
    let list = core.desktops.app_list();
    let apps: Vec<crate::desktop_apps::DesktopApp> = list.matching(&query).into_iter().cloned().collect();
    let list_rect = Rect::from_min_max(pos2(rect.min.x, field.max.y + 6.0), rect.max);
    let mut chosen = enter.then(|| apps.first().cloned()).flatten();
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(list_rect).layout(egui::Layout::top_down(egui::Align::Min)));
    if apps.is_empty() {
        let text = if query.is_empty() { "No installed apps found" } else { "Nothing matches" };
        child.painter().text(list_rect.min + vec2(16.0, 18.0), Align2::LEFT_CENTER, text, theme::ui(13.0), c(theme::TEXT_2));
    }
    egui::ScrollArea::vertical().id_salt(("your-apps", agent)).auto_shrink([false, false]).max_height(list_rect.height()).show(
        &mut child,
        |ui| {
            let width = list_rect.width() - 16.0;
            ui.spacing_mut().item_spacing.y = 0.0;
            for app in &apps {
                let (row, r) = ui.allocate_exact_size(vec2(width, APP_ROW_H), Sense::click());
                let row = Rect::from_min_size(row.min + vec2(8.0, 0.0), vec2(width - 8.0, APP_ROW_H - 2.0));
                let painter = ui.painter();
                if r.hovered() {
                    painter.rect_filled(row, CornerRadius::same(8), c([0x1A, 0x1F, 0x2A]));
                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                }
                let name = fit(painter, &app.name, theme::ui_semibold(13.5), row.width() * 0.5);
                let name_r = painter.text(
                    row.min + vec2(10.0, row.height() / 2.0),
                    Align2::LEFT_CENTER,
                    name,
                    theme::ui_semibold(13.5),
                    c(theme::TEXT),
                );
                let hint_w = row.max.x - name_r.max.x - 20.0;
                if hint_w > 40.0 && !app.blurb.is_empty() {
                    let hint = fit(painter, &app.blurb, theme::ui(12.0), hint_w);
                    painter.text(pos2(name_r.max.x + 10.0, row.center().y), Align2::LEFT_CENTER, hint, theme::ui(12.0), c(theme::TEXT_2));
                }
                if r.clicked() {
                    chosen = Some(app.clone());
                }
            }
        },
    );
    let Some(app) = chosen else { return false };
    let cwd = core.agent_dir(agent).map(|d| d.path.clone());
    match core.desktops.launch(agent, &app, cwd.as_deref()) {
        Ok(()) => core.ui.toast(core.world.time, format!("Starting {}", app.name), false),
        Err(e) => core.ui.toast(core.world.time, e, true),
    }
    core.desktops.search.clear();
    true
}
