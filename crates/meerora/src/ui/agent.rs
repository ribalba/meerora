//! The agent view (D11, D12): a rail with all agents, the app tabs and the
//! tiles, tiled or fullscreen only. Tiles resize and move (ui/tiles.rs). Each agent chooses its windows in
//! `agent.toml` (`apps`), the chat is always there. The terminal runs on a
//! PTY, the editor (ui/editor.rs) reviews the agent's changes and the
//! browser (ui/browser.rs) shows headless Chrome.

use egui::{Align2, CornerRadius, Id, LayerId, Order, Rect, Sense, Stroke, StrokeKind, Ui, pos2, vec2};
use meerora_protocol::{RingState, Role, RuntimeKind};

use meerora_protocol::App;

use super::{AppTab, ButtonStyle, Layout, View, button, fit, label, logo, open_agent, pill, portrait, status};
use crate::engine::Core;
use crate::theme::{self, c, ca};

const RAIL_W: f32 = 92.0;
const HEADER_H: f32 = 52.0;
const GAP: f32 = 8.0;

pub fn draw(ui: &mut Ui, core: &mut Core, id: &str) {
    let Some(index) = core.world.agent_index(id) else {
        core.ui.view = View::Space;
        return;
    };
    let screen = ui.ctx().content_rect();
    let painter = ui.ctx().layer_painter(LayerId::background());
    painter.rect_filled(screen, CornerRadius::ZERO, c(theme::BG));

    draw_rail(ui, core, id, screen);
    if !matches!(core.ui.view, View::Agent(_)) {
        return;
    }
    let content = content_rect(screen);
    // The agent's desktop for its own apps (D15 phase 2).
    core.desktops.ensure(id);
    let apps = apps_of(core, id);
    if let Some(app) = core.ui.open_later.take() {
        open_app(core, id, app, content);
    }
    // Tabs keep the launcher's order, wherever the tiles stand.
    let mut open = super::tiles::layout_for(core, id, &apps, content).apps();
    open.sort_by_key(tab_order);
    if !open.contains(&core.ui.tab) {
        core.ui.tab = AppTab::Chat;
    }
    // Other apps open things in the instance you used last (`target`).
    if let Some(app) = core.ui.tab.app().filter(|a| super::multi(*a)) {
        core.ui.last_instance.insert((id.to_string(), app), core.ui.tab.instance());
    }
    // A selected tab that waits in a tile it shares shows there (shell tabs).
    let tab = core.ui.tab;
    if super::tiles::layout_for(core, id, &apps, content).reveal(tab) {
        super::tiles::save_layouts(core);
    }
    draw_header(ui, core, index, screen, &open, content);
    if !matches!(core.ui.view, View::Agent(_)) {
        return;
    }

    // Esc closes an open launcher before it leaves the agent view.
    let launcher_open = core.ui.launcher;
    let mut action = None;
    let terminal_focused = match core.ui.layout {
        Layout::Fullscreen => draw_tile(ui, core, index, core.ui.tab, content, &mut action),
        Layout::Tiled => {
            let mut focused = false;
            let rects = super::tiles::layout_for(core, id, &apps, content).rects(content);
            for (tab, rect) in &rects {
                focused |= draw_tile(ui, core, index, *tab, *rect, &mut action);
            }
            super::tiles::interact(ui, core, id, &apps, content, &rects);
            focused
        }
    };
    if let Some(action) = action {
        tile_action(core, id, &apps, action, content);
    }
    super::approvals::draw_agent(ui, core, id, content);
    if !terminal_focused && !launcher_open && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
        core.ui.view = View::Space;
    }
}

/// Where the tiles go, right of the rail and below the header.
pub fn content_rect(screen: Rect) -> Rect {
    Rect::from_min_max(pos2(RAIL_W + GAP, HEADER_H + GAP), pos2(screen.max.x - GAP, screen.max.y - GAP))
}

/// The order of the tabs: the launcher's order with the instances of an app
/// by number, then the windows of the agent's desktop, oldest first.
fn tab_order(tab: &AppTab) -> (usize, u32) {
    match (tab, tab.app()) {
        (AppTab::Window(w), _) => (usize::MAX, *w),
        (_, Some(app)) => (App::ALL.iter().position(|a| *a == app).unwrap_or(usize::MAX), tab.instance()),
        (_, None) => (usize::MAX, 0),
    }
}

/// Selects the next (`step` 1) or previous (`step` -1) tab of agent `id`,
/// for Alt+Tab. A window that becomes the tab gets the keyboard.
pub fn cycle_tab(core: &mut Core, id: &str, step: isize, content: Rect) {
    let apps = apps_of(core, id);
    let mut open = super::tiles::layout_for(core, id, &apps, content).apps();
    open.sort_by_key(tab_order);
    if open.is_empty() {
        return;
    }
    let at = open.iter().position(|t| *t == core.ui.tab).unwrap_or(0) as isize;
    let next = open[(at + step).rem_euclid(open.len() as isize) as usize];
    core.ui.tab = next;
    core.desktops.focus(if let AppTab::Window(w) = next { Some(w) } else { None });
}

/// The windows an agent's `agent.toml` asks for, chat first. They are the
/// start of its layout, you open and close apps from there (ui/tiles.rs).
pub(crate) fn apps_of(core: &Core, id: &str) -> Vec<AppTab> {
    let wanted = core.agent_dir(id).map(|d| d.config.apps.clone()).unwrap_or_default();
    let mut apps = vec![AppTab::Chat];
    for app in meerora_protocol::App::ALL {
        let tab = AppTab::from_app(app);
        if tab != AppTab::Chat && wanted.contains(&app) {
            apps.push(tab);
        }
    }
    apps
}

/// Opens an app of the agent on screen with the next frame, for apps that
/// open another one, for example Files opening a file in the editor.
pub fn open_later(core: &mut Core, app: AppTab) {
    core.ui.open_later = Some(app);
}

/// Opens an app instance of agent `id` and selects it. An open instance is
/// only selected. A further instance of an app opens beside the instance of
/// it you used last.
pub fn open_app(core: &mut Core, id: &str, app: AppTab, content: Rect) {
    let apps = apps_of(core, id);
    let last = app.app().and_then(|kind| core.ui.last_instance.get(&(id.to_string(), kind)).map(|n| AppTab::of(kind, *n)));
    let layout = super::tiles::layout_for(core, id, &apps, content);
    let open = layout.apps();
    let siblings: Vec<AppTab> = open.iter().copied().filter(|t| *t != app && t.app().is_some() && t.app() == app.app()).collect();
    let near = last.filter(|t| siblings.contains(t)).or_else(|| siblings.iter().copied().max_by_key(|t| t.instance()));
    match near {
        Some(near) => layout.open_beside(app, near, content),
        None => layout.open(app, content),
    }
    core.ui.tab = app;
    super::tiles::save_layouts(core);
}

/// The open app instances of agent `id`, in tab order, without creating its
/// layout. Empty before its view first showed.
pub fn open_tabs(core: &Core, id: &str) -> Vec<AppTab> {
    let mut open = core.ui.tiles.as_ref().and_then(|l| l.get(id)).map(|l| l.apps()).unwrap_or_default();
    open.sort_by_key(tab_order);
    open
}

/// The instance of `app` where other apps open things for agent `id`, for
/// example a file from Files in the editor: the selected tab when it is
/// one, else the instance you used last while it is open, else the lowest
/// open one, else instance 1.
pub fn target(core: &Core, id: &str, app: App) -> AppTab {
    let open: Vec<AppTab> = open_tabs(core, id).into_iter().filter(|t| t.app() == Some(app)).collect();
    if core.ui.view == View::Agent(id.to_string()) && open.contains(&core.ui.tab) {
        return core.ui.tab;
    }
    let last = core.ui.last_instance.get(&(id.to_string(), app)).map(|n| AppTab::of(app, *n));
    last.filter(|t| open.contains(t)).or_else(|| open.first().copied()).unwrap_or(AppTab::from_app(app))
}

/// A new instance of `app` for agent `id`: the lowest number that is not
/// open. Closed numbers come back, so handles stay short.
pub fn next_instance(core: &Core, id: &str, app: App) -> AppTab {
    let open = open_tabs(core, id);
    (1..).map(|n| AppTab::of(app, n)).find(|t| !open.contains(t)).unwrap_or(AppTab::from_app(app))
}

/// What a tile asks of the view once every tile drew.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TileAction {
    /// The cross on its head.
    Close(AppTab),
    /// A tab of the shell strip: show `tab` in the tile that shows `tile`.
    Show { tab: AppTab, tile: AppTab },
    /// The "+" of the shell strip: a new shell as a tab of the tile that shows `tile`.
    NewShell { tile: AppTab },
    /// "Own tile" in the menu of a shell tab: out of the shared tile.
    Split(AppTab),
}

fn tile_action(core: &mut Core, id: &str, apps: &[AppTab], action: TileAction, content: Rect) {
    match action {
        TileAction::Close(app) => close_app(core, id, apps, app, content),
        // In fullscreen there are no tiles to share, so a shell tab only selects.
        TileAction::Show { tab, tile } => {
            if core.ui.layout == Layout::Tiled && super::tiles::layout_for(core, id, apps, content).join(tab, tile) {
                super::tiles::save_layouts(core);
            }
            core.ui.tab = tab;
        }
        TileAction::NewShell { tile } => {
            let tab = next_instance(core, id, App::Terminal);
            super::tiles::layout_for(core, id, apps, content).join(tab, tile);
            core.ui.tab = tab;
            super::tiles::save_layouts(core);
        }
        TileAction::Split(tab) => {
            if super::tiles::layout_for(core, id, apps, content).split(tab, content) {
                super::tiles::save_layouts(core);
            }
            core.ui.tab = tab;
        }
    }
}

/// Closes an app instance of agent `id`. A closed terminal ends its shell
/// and a closed browser closes its page, the agent's Chrome ends with its
/// last page. The other instances keep theirs. The editor keeps its open
/// files, so nothing unsaved is lost. Another instance of the same app
/// becomes the tab when there is one.
fn close_app(core: &mut Core, id: &str, apps: &[AppTab], app: AppTab, content: Rect) {
    // A window is asked to close. Its tile goes when the window is gone (ui/window.rs).
    if let AppTab::Window(w) = app {
        super::window::request_close(core, w);
        return;
    }
    let layout = super::tiles::layout_for(core, id, apps, content);
    let mate = layout.group(app).into_iter().find(|t| *t != app);
    if !layout.close(app) {
        return;
    }
    // In a shared tile the tab that shows now takes over.
    let heir = mate.and_then(|m| layout.shown(m));
    match app {
        AppTab::Terminal(n) => core.close_terminal(id, n),
        AppTab::Browser(n) => core.browsers.close_instance(id, n),
        _ => {}
    }
    if core.ui.tab == app {
        let sibling = open_tabs(core, id).into_iter().find(|t| t.app().is_some() && t.app() == app.app());
        core.ui.tab = heir.or(sibling).unwrap_or(AppTab::Chat);
    }
    super::tiles::save_layouts(core);
}

fn draw_rail(ui: &mut Ui, core: &mut Core, current: &str, screen: Rect) {
    let rail = Rect::from_min_max(screen.min, pos2(RAIL_W, screen.max.y));
    let mut go: Option<Option<String>> = None;
    let mut new_agent = false;
    egui::Area::new(Id::new("rail")).fade_in(false).fixed_pos(rail.min).order(Order::Middle).show(ui.ctx(), |ui| {
        ui.allocate_exact_size(rail.size(), Sense::hover());
        let painter = ui.painter();
        painter.rect_filled(rail, CornerRadius::ZERO, c([0x0E, 0x11, 0x17]));
        super::vline(painter, rail.max.x, rail.min.y, rail.max.y);

        let space = Rect::from_min_size(pos2(8.0, 10.0), vec2(RAIL_W - 16.0, 62.0));
        let r = ui.interact(space, Id::new("rail-space"), Sense::click());
        let painter = ui.painter();
        if r.hovered() {
            painter.rect_filled(space, CornerRadius::same(12), c([0x16, 0x1B, 0x26]));
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        logo(painter, pos2(space.center().x, space.min.y + 22.0), 22.0);
        painter.text(pos2(space.center().x, space.min.y + 46.0), Align2::CENTER_CENTER, "Space", theme::ui_semibold(13.0), c(theme::TEXT));
        if r.clicked() {
            go = Some(None);
        }
        super::hline(painter, space.max.y + 8.0, 16.0, RAIL_W - 16.0);

        let mut y = space.max.y + 18.0;
        for (i, a) in core.world.agents.iter().enumerate() {
            let slot = Rect::from_min_size(pos2(8.0, y), vec2(RAIL_W - 16.0, 92.0));
            let r = ui.interact(slot, Id::new(("rail-agent", i)), Sense::click());
            let painter = ui.painter();
            let selected = a.id == current;
            if selected || r.hovered() {
                painter.rect_filled(slot, CornerRadius::same(12), if selected { c([0x16, 0x1B, 0x26]) } else { c([0x13, 0x17, 0x20]) });
            }
            portrait(painter, Rect::from_center_size(pos2(slot.center().x, slot.min.y + 32.0), vec2(56.0, 56.0)), core, a);
            painter.text(
                pos2(slot.center().x, slot.min.y + 66.0),
                Align2::CENTER_CENTER,
                &a.name,
                theme::ui_semibold(13.0),
                c(theme::TEXT),
            );
            let (st, color) = status(a);
            let st = match a.ring {
                RingState::Resting if !a.system => a.activity.split_whitespace().last().unwrap_or("").to_uppercase(),
                _ => st,
            };
            let st = fit(painter, &st, theme::mono(9.5), slot.width() - 6.0);
            painter.text(pos2(slot.center().x, slot.min.y + 81.0), Align2::CENTER_CENTER, st, theme::mono(9.5), c(color));
            if a.ring == RingState::NeedsYou && !selected {
                painter.rect_filled(
                    Rect::from_min_size(pos2(slot.max.x - 18.0, slot.min.y + 6.0), vec2(10.0, 10.0)),
                    CornerRadius::same(2),
                    c(theme::NEEDS_YOU),
                );
            }
            if r.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
            if r.clicked() && !selected {
                go = Some(Some(a.id.clone()));
            }
            y += 96.0;
        }
        let plus = Rect::from_min_size(pos2(20.0, rail.max.y - 64.0), vec2(RAIL_W - 40.0, 44.0));
        let r = ui.interact(plus, Id::new("rail-new"), Sense::click());
        let painter = ui.painter();
        painter.rect(
            plus,
            CornerRadius::same(12),
            if r.hovered() { c([0x16, 0x1B, 0x26]) } else { egui::Color32::TRANSPARENT },
            Stroke::new(1.0, c(theme::BORDER)),
            StrokeKind::Inside,
        );
        painter.text(plus.center(), Align2::CENTER_CENTER, "+", theme::ui(22.0), c(theme::TEXT_2));
        if r.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        if r.clicked() {
            new_agent = true;
        }
    });
    if new_agent {
        super::open_builder(core);
        return;
    }
    match go {
        Some(None) => core.ui.view = View::Space,
        Some(Some(id)) => open_agent(core, &id),
        None => {}
    }
}

fn draw_header(ui: &mut Ui, core: &mut Core, index: usize, screen: Rect, open: &[AppTab], content: Rect) {
    let bar = Rect::from_min_max(pos2(RAIL_W, 0.0), pos2(screen.max.x, HEADER_H));
    let mut edit_agent = false;
    egui::Area::new(Id::new("agent-header")).fade_in(false).fixed_pos(bar.min).order(Order::Middle).show(ui.ctx(), |ui| {
        ui.allocate_exact_size(bar.size(), Sense::hover());
        let a = &core.world.agents[index];
        let painter = ui.painter();
        painter.rect_filled(bar, CornerRadius::ZERO, c(theme::BG));
        super::hline(painter, bar.max.y, bar.min.x, bar.max.x);
        let name = painter.text(pos2(bar.min.x + 16.0, bar.center().y), Align2::LEFT_CENTER, &a.name, theme::ui_bold(17.0), c(theme::TEXT));
        let station = a.station().map(|s| format!("at the {}", s.replace('_', " "))).unwrap_or_default();
        let sub = painter.text(pos2(name.max.x + 10.0, bar.center().y), Align2::LEFT_CENTER, station, theme::ui(14.0), c(theme::TEXT_2));
        let (st, color) = status(a);
        let st = if a.activity.is_empty() { st } else { format!("{st} · {}", a.activity.to_uppercase()) };
        let st = fit(painter, &st, theme::mono(10.5), 300.0);
        let pill_w = painter.layout_no_wrap(st.clone(), theme::mono(10.5), c(color)).size().x + 14.0;
        let st_r = pill(painter, pos2(sub.max.x + 12.0 + pill_w, bar.center().y), &st, color);
        let badge = super::index::header_badge(painter, core, &a.id, pos2(st_r.max.x + 8.0, bar.center().y), bar.center().x);
        let left_end = badge.map_or(st_r.max.x, |b| b.max.x) + 16.0;

        // App tabs, only the windows this agent has, between the agent's line
        // and the buttons on the right. They narrow before they overlap.
        // The "+" after them opens the launcher, which also lists the apps
        // whose tab does not fit.
        let external = super::index::external(core, &core.world.agents[index].id).is_some();
        let right_start = bar.max.x - 16.0 - 220.0 - 12.0 - if external { 16.0 } else { 104.0 + 10.0 + 40.0 + 16.0 };
        const PLUS_W: f32 = 44.0;
        let room = (right_start - left_end - PLUS_W).max(0.0);
        let fit_n = ((room / 78.0).floor() as usize).max(1);
        let mut shown: Vec<AppTab> = open.iter().copied().take(fit_n).collect();
        if !shown.contains(&core.ui.tab)
            && let Some(last) = shown.last_mut()
        {
            *last = core.ui.tab;
        }
        let n = shown.len() as f32;
        let tab_w = (room / n).clamp(78.0, 104.0);
        let mut x = (bar.center().x - (tab_w * n + PLUS_W) / 2.0 + 60.0).min(right_start - tab_w * n - PLUS_W).max(left_end);
        for tab in shown {
            let rect = Rect::from_min_size(pos2(x, bar.center().y - 17.0), vec2(tab_w - 6.0, 34.0));
            let style = if core.ui.tab == tab { ButtonStyle::Selected } else { ButtonStyle::Ghost };
            let title = fit(ui.painter(), &super::tab_title(core, tab), theme::ui(14.5), rect.width() - 14.0);
            if button(ui, rect, &title, style).clicked() {
                core.ui.tab = tab;
            }
            x += tab_w;
        }
        let plus = Rect::from_min_size(pos2(x, bar.center().y - 17.0), vec2(34.0, 34.0));
        let plus_style = if core.ui.launcher { ButtonStyle::Selected } else { ButtonStyle::Ghost };
        if button(ui, plus, "+", plus_style).on_hover_text("Open an app").clicked() {
            core.ui.launcher = !core.ui.launcher;
        }
        x += PLUS_W;
        if core.ui.launcher {
            let id = core.world.agents[index].id.clone();
            launcher(ui, core, &id, plus, open, content);
        }
        let apps = open.to_vec();

        // Tiled or fullscreen, nothing in between (D11).
        let seg = Rect::from_min_size(pos2(bar.max.x - 16.0 - 220.0, bar.center().y - 19.0), vec2(220.0, 38.0));
        let edit = Rect::from_min_size(pos2(seg.min.x - 12.0 - 104.0, seg.min.y), vec2(104.0, 38.0));
        // External agents come from their manifest, the builder cannot change them (D25).
        if super::index::external(core, &core.world.agents[index].id).is_none()
            && button(ui, edit, "Edit agent", ButtonStyle::Quiet).clicked()
        {
            edit_agent = true;
        }
        // What the agent may do, each with Revoke (D8). External agents hold nothing (D25).
        if super::index::external(core, &core.world.agents[index].id).is_none() {
            let id = core.world.agents[index].id.clone();
            let tabs = (x - tab_w * apps.len() as f32, x - 6.0);
            super::approvals::access_button(ui, core, &id, tabs, edit.min.x - 10.0, bar.center().y);
        }
        let painter = ui.painter();
        painter.rect(seg, CornerRadius::same(12), c([0x0E, 0x11, 0x17]), Stroke::new(1.0, c(theme::BORDER)), StrokeKind::Inside);
        let half = Rect::from_min_size(seg.min + vec2(3.0, 3.0), vec2(seg.width() / 2.0 - 3.0, seg.height() - 6.0));
        let tiled = button(ui, half, "Tiled", if core.ui.layout == Layout::Tiled { ButtonStyle::Selected } else { ButtonStyle::Ghost });
        let full = button(
            ui,
            half.translate(vec2(half.width(), 0.0)),
            "Fullscreen",
            if core.ui.layout == Layout::Fullscreen { ButtonStyle::Selected } else { ButtonStyle::Ghost },
        );
        if tiled.clicked() {
            core.ui.layout = Layout::Tiled;
        }
        if full.clicked() {
            core.ui.layout = Layout::Fullscreen;
        }
    });
    if edit_agent {
        let id = core.world.agents[index].id.clone();
        super::builder::open_edit(core, &id);
    }
}

/// Draws one tile. Returns true when the terminal, the editor or the browser has keyboard focus.
fn draw_tile(ui: &mut Ui, core: &mut Core, index: usize, tab: AppTab, rect: Rect, action: &mut Option<TileAction>) -> bool {
    let mut focused = false;
    let area_id = Id::new(("tile", tab));
    egui::Area::new(area_id).fade_in(false).fixed_pos(rect.min).order(Order::Middle).show(ui.ctx(), |ui| {
        ui.allocate_exact_size(rect.size(), Sense::hover());
        // Tiles can be dragged narrow, so nothing may paint outside its own tile.
        ui.set_clip_rect(rect);
        let selected = core.ui.tab == tab;
        let painter = ui.painter();
        let border = if selected && core.ui.layout == Layout::Tiled { c([0x33, 0x3A, 0x48]) } else { c([0x20, 0x24, 0x2E]) };
        painter.rect(rect, CornerRadius::same(14), c(theme::PANEL), Stroke::new(1.0, border), StrokeKind::Inside);
        let head = Rect::from_min_size(rect.min, vec2(rect.width(), 38.0));
        // Window titles are long, they leave room for the head controls.
        let title = match tab {
            AppTab::Window(_) => fit(painter, &super::tab_title(core, tab), theme::ui_semibold(14.5), (head.width() * 0.45).max(60.0)),
            _ => tab.label(),
        };
        painter.text(pos2(head.min.x + 16.0, head.center().y), Align2::LEFT_CENTER, title, theme::ui_semibold(14.5), c(theme::TEXT));
        super::hline(painter, head.max.y, rect.min.x, rect.max.x);
        // Clicking the head selects the tile, dragging it moves the tile (ui/tiles.rs).
        let head_click = ui.interact(head, Id::new(("tile-head", tab)), Sense::click_and_drag());
        if head_click.clicked() {
            core.ui.tab = tab;
        }
        if head_click.drag_started() && core.ui.layout == Layout::Tiled {
            core.ui.tile_drag = Some(tab);
        }
        if head_click.hovered() && core.ui.layout == Layout::Tiled && core.ui.tile_drag.is_none() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
        }
        if head_click.double_clicked() {
            core.ui.layout = if core.ui.layout == Layout::Tiled { Layout::Fullscreen } else { Layout::Tiled };
        }
        let body = Rect::from_min_max(pos2(rect.min.x + 1.0, head.max.y + 1.0), pos2(rect.max.x - 1.0, rect.max.y - 1.0));
        // Every app but the chat closes with the cross at the end of its head.
        // The app draws its own head controls left of it.
        let head = if tab == AppTab::Chat {
            head
        } else {
            let cross = Rect::from_center_size(pos2(head.max.x - 20.0, head.center().y), vec2(26.0, 26.0));
            if close_button(ui, cross, Id::new(("tile-close", tab)))
                .on_hover_text(format!("Close {}", super::tab_title(core, tab)))
                .clicked()
            {
                *action = Some(TileAction::Close(tab));
            }
            Rect::from_min_max(head.min, pos2(cross.min.x - 4.0, head.max.y))
        };
        // The apps put their head text a fixed distance after their title.
        // From instance 2 on the title is longer by its number, so the head
        // they get starts that much later.
        let head = match tab {
            AppTab::Window(_) => head,
            _ if tab.instance() >= 2 => {
                let width = |text: String| ui.painter().layout_no_wrap(text, theme::ui_semibold(14.5), c(theme::TEXT)).size().x;
                let extra = width(tab.label()) - width(tab.title().to_string());
                Rect::from_min_max(pos2((head.min.x + extra).min(head.max.x), head.min.y), head.max)
            }
            _ => head,
        };
        match tab {
            AppTab::Terminal(n) => focused = terminal_tile(ui, core, index, n, head, body, action),
            AppTab::Chat => agent_tile(ui, core, index, body),
            AppTab::Editor(n) => focused = super::editor::tile(ui, core, index, n, head, body),
            AppTab::Browser(n) => focused = super::browser::tile(ui, core, index, n, head, body),
            AppTab::Files(n) => focused = super::apps::files::tile(ui, core, index, n, head, body),
            AppTab::Viewer(n) => focused = super::apps::viewer::tile(ui, core, index, n, head, body),
            AppTab::Changes => focused = super::apps::changes::tile(ui, core, index, head, body),
            AppTab::Git(n) => focused = super::apps::git::tile(ui, core, index, n, head, body),
            AppTab::Activity => focused = super::apps::activity::tile(ui, core, index, head, body),
            AppTab::Window(w) => focused = super::window::tile(ui, core, w, head, body),
        }
    });
    focused
}

/// A small cross button.
fn close_button(ui: &mut Ui, rect: Rect, id: Id) -> egui::Response {
    let response = ui.interact(rect, id, Sense::click());
    let painter = ui.painter();
    if response.hovered() {
        painter.rect_filled(rect, CornerRadius::same(7), c([0x22, 0x27, 0x33]));
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let color = if response.hovered() { c(theme::TEXT) } else { c(theme::TEXT_2) };
    let (center, r) = (rect.center(), 4.5);
    painter.line_segment([center + vec2(-r, -r), center + vec2(r, r)], Stroke::new(1.5, color));
    painter.line_segment([center + vec2(-r, r), center + vec2(r, -r)], Stroke::new(1.5, color));
    response
}

/// The app launcher under the "+" of the header: every app with a line about
/// it. A click opens a closed app or selects an open one (see [`target`]).
/// Apps that run as several instances have a "New" button, which opens
/// another one. Below them, "Your apps" starts installed apps on the agent's
/// desktop (ui/window.rs).
fn launcher(ui: &mut Ui, core: &mut Core, id: &str, plus: Rect, open: &[AppTab], content: Rect) {
    const ROW_H: f32 = 52.0;
    let apps: Vec<App> = App::ALL.to_vec();
    let builtin_h = 44.0 + ROW_H * apps.len() as f32 + 12.0;
    let screen = ui.ctx().content_rect();
    let top = plus.max.y + 8.0;
    let rows =
        ((screen.max.y - top - 12.0 - builtin_h - super::window::YOUR_APPS_HEAD - 8.0) / super::window::APP_ROW_H).floor().clamp(3.0, 8.0);
    let size = vec2(360.0, builtin_h + super::window::YOUR_APPS_HEAD + rows * super::window::APP_ROW_H + 8.0);
    let pos = pos2((plus.min.x - 12.0).min(screen.max.x - size.x - 12.0), top);
    let mut chosen = None;
    let mut new = None;
    let mut started = false;
    egui::Area::new(Id::new(("launcher", id))).fade_in(false).fixed_pos(pos).order(Order::Foreground).show(ui.ctx(), |ui| {
        let rect = Rect::from_min_size(pos, size);
        ui.allocate_exact_size(size, Sense::click());
        let painter = ui.painter();
        super::panel(painter, rect, 14);
        painter.text(rect.min + vec2(16.0, 22.0), Align2::LEFT_CENTER, "APPS", theme::mono(10.5), c(theme::TEXT_2));
        for (i, app) in apps.iter().enumerate() {
            let row = Rect::from_min_size(rect.min + vec2(8.0, 40.0 + ROW_H * i as f32), vec2(size.x - 16.0, ROW_H - 4.0));
            let r = ui.interact(row, Id::new(("launcher-app", *app)), Sense::click());
            let painter = ui.painter();
            if r.hovered() {
                painter.rect_filled(row, CornerRadius::same(10), c([0x1A, 0x1F, 0x2A]));
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
            let tab = AppTab::from_app(*app);
            painter.text(row.min + vec2(10.0, 16.0), Align2::LEFT_CENTER, tab.title(), theme::ui_semibold(14.5), c(theme::TEXT));
            let mut right = row.max.x - 10.0;
            if super::multi(*app) {
                let b = Rect::from_min_size(pos2(right - 54.0, row.center().y - 13.0), vec2(54.0, 26.0));
                let hint = format!("Open another {}", tab.title().to_lowercase());
                if button(ui, b, "New", ButtonStyle::Quiet).on_hover_text(hint).clicked() {
                    new = Some(*app);
                }
                right = b.min.x - 8.0;
            }
            let painter = ui.painter();
            let blurb = fit(painter, tab.blurb(), theme::ui(12.5), right - row.min.x - 14.0);
            painter.text(row.min + vec2(10.0, 35.0), Align2::LEFT_CENTER, blurb, theme::ui(12.5), c(theme::TEXT_2));
            let count = open.iter().filter(|t| t.app() == Some(*app)).count();
            if count > 0 {
                let text = if count > 1 { format!("{count} OPEN") } else { "OPEN".to_string() };
                pill(painter, pos2(right, row.min.y + 16.0), &text, theme::WORKING);
            }
            if r.clicked() {
                chosen = Some(*app);
            }
        }
        let yours = Rect::from_min_max(pos2(rect.min.x, rect.min.y + builtin_h - 4.0), rect.max - vec2(0.0, 8.0));
        started = super::window::your_apps(ui, core, id, yours);
    });
    // Inside means inside the launcher's rectangle. The area's own response is
    // not hovered while a child such as the search field takes the pointer, so
    // asking it would close the launcher on the first click into the field.
    let launcher_rect = Rect::from_min_size(pos, size);
    let inside = ui.input(|i| i.pointer.interact_pos()).is_some_and(|p| launcher_rect.contains(p) || plus.contains(p));
    let clicked_outside = ui.input(|i| i.pointer.any_click()) && !inside;
    if started {
        core.ui.launcher = false;
    } else if let Some(app) = new {
        core.ui.launcher = false;
        let tab = next_instance(core, id, app);
        open_app(core, id, tab, content);
    } else if let Some(app) = chosen {
        core.ui.launcher = false;
        let tab = target(core, id, app);
        open_app(core, id, tab, content);
    } else if clicked_outside || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
        core.ui.launcher = false;
    }
}

/// Terminal `n` of agent `index`. The agent types into terminal 1 only, so
/// the others never show it typing. The head has the shell strip.
fn terminal_tile(ui: &mut Ui, core: &mut Core, index: usize, n: u32, head: Rect, body: Rect, action: &mut Option<TileAction>) -> bool {
    let now = core.world.time;
    let id = core.world.agents[index].id.clone();
    let name = core.world.agents[index].name.clone();
    // The mock types into terminal 1, a real agent into any terminal through the app tools (D30).
    let typing = (n == 1 && core.world.agents[index].is_typing(now)) || crate::agent_apps::doing(core, &id, AppTab::Terminal(n)).is_some();
    let Some(tile) = core.terminal(&id, n) else {
        placeholder(ui, body, "The terminal could not start", "See the log for the error from the PTY.");
        return false;
    };
    let taken_over = tile.taken_over;
    // D12: show who controls the window, with a take over button.
    let (chip, color) = if taken_over {
        ("YOU · TAKEN OVER".to_string(), theme::NEEDS_YOU)
    } else if typing {
        (format!("{} IS TYPING", name.to_uppercase()), theme::WORKING)
    } else {
        ("YOU".to_string(), [0xB9, 0xBF, 0xCC])
    };
    let mut right = head.max.x - 12.0;
    if typing || taken_over {
        let b = Rect::from_min_size(pos2(right - 96.0, head.center().y - 13.0), vec2(96.0, 26.0));
        let text = if taken_over { "Hand back" } else { "Take over" };
        if button(ui, b, text, ButtonStyle::Quiet).clicked()
            && let Some(tile) = core.terminal(&id, n)
        {
            tile.taken_over = !taken_over;
        }
        right = b.min.x - 10.0;
    }
    let chip_r = pill(ui.painter(), pos2(right, head.center().y), &chip, color);
    shell_strip(ui, core, &id, n, head, chip_r.min.x - 10.0, action);
    let Some(tile) = core.terminal(&id, n) else { return false };
    let focused = super::terminal::show(ui, body, Id::new(("term", super::instance_key(&id, n))), &tile.term);
    if focused && typing && !tile.taken_over {
        // Typing yourself takes over, as in screen 2.
        let typed = ui.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Text(_) | egui::Event::Key { pressed: true, .. })));
        if typed {
            tile.taken_over = true;
        }
    }
    focused
}

/// The shell strip in the head of the tile of terminal `n`, between its
/// title and `right`: a tab for each of the agent's terminals and a "+" for
/// a new shell in this tile. A tab of this tile shows it here, a tab of a
/// terminal in another tile moves that terminal here. The strip shows once
/// the agent has more than one terminal, the "+" always.
fn shell_strip(ui: &mut Ui, core: &Core, id: &str, n: u32, head: Rect, right: f32, action: &mut Option<TileAction>) {
    const H: f32 = 22.0;
    const PLUS_W: f32 = 24.0;
    let this = AppTab::Terminal(n);
    let terminals: Vec<AppTab> = open_tabs(core, id).into_iter().filter(|t| matches!(t, AppTab::Terminal(_))).collect();
    let group = core.ui.tiles.as_ref().and_then(|l| l.get(id)).map(|l| l.group(this)).unwrap_or_default();
    // The head already starts after the instance number (see `draw_tile`).
    let title_w = ui.painter().layout_no_wrap(this.title().to_string(), theme::ui_semibold(14.5), c(theme::TEXT)).size().x;
    let left = head.min.x + 16.0 + title_w + 12.0;
    let room = right - left;
    let width = |t: &AppTab| if t.instance() >= 10 { 34.0 } else { 26.0 };
    let fits = |tabs: &[AppTab]| tabs.iter().map(|t| width(t) + 4.0).sum::<f32>() + PLUS_W <= room;
    // When all do not fit, the tabs of this tile come first.
    let shown: Vec<AppTab> = if terminals.len() < 2 {
        Vec::new()
    } else if fits(&terminals) {
        terminals
    } else if fits(&group) && group.len() > 1 {
        terminals.into_iter().filter(|t| group.contains(t)).collect()
    } else {
        Vec::new()
    };
    if room < PLUS_W {
        return;
    }
    let mut x = left;
    for tab in &shown {
        let rect = Rect::from_min_size(pos2(x, head.center().y - H / 2.0), vec2(width(tab), H));
        let mine = group.contains(tab);
        let style = if *tab == this {
            StripStyle::Selected
        } else if mine {
            StripStyle::Normal
        } else {
            StripStyle::Elsewhere
        };
        let hint = match style {
            StripStyle::Selected => format!("{} shows here", tab.label()),
            StripStyle::Normal => format!("Show {} here", tab.label()),
            StripStyle::Elsewhere => format!("{} has its own tile. Click to show it here.", tab.label()),
        };
        let response =
            strip_tab(ui, rect, Id::new(("shell-tab", id, n, tab.instance())), &tab.instance().to_string(), style).on_hover_text(hint);
        if response.clicked() && *tab != this {
            *action = Some(TileAction::Show { tab: *tab, tile: this });
        }
        if mine && group.len() > 1 {
            response.context_menu(|ui| {
                ui.set_min_width(150.0);
                if ui.button("Own tile").clicked() {
                    *action = Some(TileAction::Split(*tab));
                    ui.close();
                }
            });
        }
        x += width(tab) + 4.0;
    }
    let plus = Rect::from_min_size(pos2(x, head.center().y - H / 2.0), vec2(PLUS_W, H));
    if strip_tab(ui, plus, Id::new(("shell-new", id, n)), "+", StripStyle::Normal).on_hover_text("New shell in this tile").clicked() {
        *action = Some(TileAction::NewShell { tile: this });
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum StripStyle {
    Selected,
    Normal,
    /// A terminal in another tile.
    Elsewhere,
}

/// One small tab of the shell strip.
fn strip_tab(ui: &mut Ui, rect: Rect, id: Id, label: &str, style: StripStyle) -> egui::Response {
    let response = ui.interact(rect, id, Sense::click());
    let hovered = response.hovered();
    let (fill, stroke, text) = match style {
        StripStyle::Selected => (c([0x1E, 0x23, 0x2E]), Stroke::new(1.0, c([0x3A, 0x42, 0x52])), c(theme::TEXT)),
        StripStyle::Normal => (if hovered { c([0x1A, 0x1F, 0x2A]) } else { egui::Color32::TRANSPARENT }, Stroke::NONE, c(theme::TEXT_2)),
        StripStyle::Elsewhere => (
            if hovered { c([0x1A, 0x1F, 0x2A]) } else { egui::Color32::TRANSPARENT },
            Stroke::new(1.0, c([0x2A, 0x30, 0x3D])),
            c([0x6B, 0x72, 0x82]),
        ),
    };
    let painter = ui.painter();
    painter.rect(rect, CornerRadius::same(7), fill, stroke, StrokeKind::Inside);
    painter.text(rect.center(), Align2::CENTER_CENTER, label, theme::mono(11.5), text);
    if hovered {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    response
}

fn agent_tile(ui: &mut Ui, core: &mut Core, index: usize, body: Rect) {
    let a = &core.world.agents[index];
    let painter = ui.painter();
    let p = Rect::from_min_size(body.min + vec2(16.0, 16.0), vec2(96.0, 96.0));
    painter.rect_filled(p, CornerRadius::same(14), c([0x16, 0x1A, 0x23]));
    portrait(painter, p, core, a);
    let x = p.max.x + 16.0;
    painter.text(pos2(x, p.min.y + 14.0), Align2::LEFT_TOP, &a.name, theme::ui_bold(19.0), c(theme::TEXT));
    let station = a.station().map(|s| format!("At the {}", s.replace('_', " "))).unwrap_or_default();
    painter.text(pos2(x, p.min.y + 42.0), Align2::LEFT_TOP, station, theme::ui(14.0), c(theme::TEXT_2));
    let (st, color) = status(a);
    let line = if a.activity.is_empty() { st } else { format!("{} · {st}", a.activity.to_uppercase()) };
    let line = fit(painter, &line, theme::mono(10.5), body.max.x - x - 16.0);
    painter.text(pos2(x, p.min.y + 66.0), Align2::LEFT_TOP, line, theme::mono(10.5), c(color));
    if let (RingState::Working, Some(progress)) = (a.ring, a.progress) {
        let bar = Rect::from_min_size(pos2(x, p.min.y + 86.0), vec2(body.max.x - x - 16.0, 4.0));
        painter.rect_filled(bar, CornerRadius::same(2), c(theme::BORDER));
        let mut done = bar;
        done.set_width(bar.width() * progress.clamp(0.0, 1.0));
        painter.rect_filled(done, CornerRadius::same(2), c(theme::WORKING));
    }
    super::hline(painter, p.max.y + 16.0, body.min.x, body.max.x);

    let privacy_h = super::index::privacy_height(core, &a.id, painter, body.width() - 24.0);
    let input_h = 76.0 + privacy_h;
    let actions_h = 30.0 + 22.0 * a.actions.len().clamp(1, 5) as f32;
    let input_top = body.max.y - input_h;
    let actions_top = input_top - actions_h;
    let chat = Rect::from_min_max(pos2(body.min.x + 12.0, p.max.y + 24.0), pos2(body.max.x - 12.0, actions_top - 8.0));
    conversation(ui, core, index, chat);

    let a = &core.world.agents[index];
    let painter = ui.painter();
    super::hline(painter, actions_top, body.min.x, body.max.x);
    let mut y = actions_top + 12.0;
    label(painter, pos2(body.min.x + 16.0, y), "ACTIONS ON YOUR APPS");
    y += 22.0;
    if a.actions.is_empty() {
        painter.text(pos2(body.min.x + 16.0, y), Align2::LEFT_TOP, "Nothing yet", theme::ui(13.5), c(theme::TEXT_2));
    }
    for entry in a.actions.iter().rev().take(5) {
        let ago = core.world.time - entry.at;
        let when = if ago < 60.0 { format!("{}s", ago as i32) } else { format!("{}m", (ago / 60.0) as i32) };
        let when_r = painter.text(pos2(body.max.x - 16.0, y), Align2::RIGHT_TOP, when, theme::mono(10.5), c(theme::TEXT_2));
        let text = format!("{} {}", entry.tool, entry.summary);
        let text = fit(painter, &text, theme::mono(11.5), when_r.min.x - body.min.x - 28.0);
        painter.text(pos2(body.min.x + 16.0, y), Align2::LEFT_TOP, text, theme::mono(11.5), c([0xC9, 0xCE, 0xD9]));
        y += 22.0;
    }
    super::index::privacy_note(ui.painter(), core, &core.world.agents[index].id, body.min.x + 12.0, input_top + 10.0, body.width() - 24.0);
    let input = Rect::from_min_max(pos2(body.min.x + 12.0, input_top + 10.0 + privacy_h), pos2(body.max.x - 12.0, body.max.y - 12.0));
    chat_input(ui, core, index, input);
}

/// The conversation with the agent, newest at the bottom (screen 2).
fn conversation(ui: &mut Ui, core: &Core, index: usize, rect: Rect) {
    let a = &core.world.agents[index];
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect).layout(egui::Layout::top_down(egui::Align::Min)));
    egui::ScrollArea::vertical().id_salt(("chat", &a.id)).stick_to_bottom(true).auto_shrink([false, false]).show(&mut child, |ui| {
        let width = rect.width() - 14.0;
        if a.messages.is_empty() {
            let hint = match core.agent_dir(&a.id).and_then(|d| d.config.runtime.as_ref()).map(|r| r.kind) {
                Some(RuntimeKind::Mock) | None => {
                    format!("{} is a mock agent played by the scenario. Choose a model under Edit agent to talk to it.", a.name)
                }
                Some(_) => format!("Tell {} what to do. It works in its own directory.", a.name),
            };
            ui.add_space(6.0);
            ui.label(egui::RichText::new(hint).font(theme::ui(13.5)).color(c(theme::TEXT_2)));
        }
        for m in &a.messages {
            bubble(ui, m, width);
            ui.add_space(8.0);
        }
        if let Some(run) = &a.run
            && a.messages.last().is_none_or(|m| m.done || m.role != Role::Agent)
        {
            let dots = ".".repeat(1 + (core.world.time * 2.0) as usize % 3);
            let source = if run.trigger == meerora_protocol::Trigger::Schedule { "scheduled: " } else { "" };
            let task = run.task.lines().next().unwrap_or_default();
            let line = format!("{} is working on {source}{task}{dots}", a.name);
            ui.add(egui::Label::new(egui::RichText::new(line).font(theme::mono(11.0)).color(c(theme::WORKING))).wrap());
        }
    });
}

pub fn bubble(ui: &mut Ui, m: &crate::world::ChatMessage, width: f32) {
    let (fill, stroke, align, font) = match m.role {
        Role::User => (c([0x1C, 0x21, 0x2C]), Stroke::NONE, egui::Align::Max, theme::ui(14.5)),
        Role::Agent => (c([0x1A, 0x17, 0x10]), Stroke::new(1.0, c([0x3A, 0x2E, 0x18])), egui::Align::Min, theme::ui(14.5)),
        Role::System => (ca([0xE0, 0x67, 0x5A], 18), Stroke::new(1.0, ca([0xE0, 0x67, 0x5A], 70)), egui::Align::Min, theme::mono(12.0)),
    };
    ui.with_layout(egui::Layout::top_down(align), |ui| {
        egui::Frame::new()
            .fill(fill)
            .stroke(stroke)
            .corner_radius(CornerRadius::same(12))
            .inner_margin(egui::Margin::symmetric(12, 9))
            .show(ui, |ui| {
                ui.set_max_width(width * 0.88);
                let text = if m.text.is_empty() && !m.done { "…".to_string() } else { m.text.clone() };
                ui.add(egui::Label::new(egui::RichText::new(text).font(font).color(c(theme::TEXT))).wrap());
            });
    });
}

fn chat_input(ui: &mut Ui, core: &mut Core, index: usize, rect: Rect) {
    let id = core.world.agents[index].id.clone();
    let name = core.world.agents[index].name.clone();
    let running = core.world.agents[index].run.is_some();
    let kind = core.agent_dir(&id).and_then(|d| d.config.runtime.as_ref()).map(|r| r.kind);
    let enabled = !matches!(kind, Some(RuntimeKind::Mock) | None) && core.ui.daemon_connected;
    let painter = ui.painter();
    painter.rect(rect, CornerRadius::same(22), c([0x16, 0x1A, 0x23]), Stroke::new(1.0, c(theme::BORDER)), StrokeKind::Inside);
    let button_w = if running { 78.0 } else { 0.0 };
    // Microphone and speaker toggle of local voice (D16, ui/voice.rs).
    let voice_w = super::voice::chat_controls_width(core);
    let field = Rect::from_min_max(rect.min + vec2(16.0, 8.0), pos2(rect.max.x - 16.0 - button_w - voice_w, rect.max.y - 8.0));
    let hint = if !core.ui.daemon_connected {
        "The agent daemon is not running".to_string()
    } else if enabled {
        format!("Tell {name} what to do next")
    } else {
        format!("{name} is a mock agent")
    };
    let draft = core.ui.drafts.entry(id.clone()).or_default();
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(field));
    child.add_enabled_ui(enabled, |ui| {
        let edit = egui::TextEdit::multiline(draft)
            .id(Id::new(("chat-input", &id)))
            .hint_text(egui::RichText::new(hint).color(c([0x6B, 0x72, 0x82])))
            .font(theme::ui(14.5))
            .frame(egui::Frame::NONE)
            .desired_rows(2)
            .desired_width(field.width())
            .return_key(Some(egui::KeyboardShortcut::new(egui::Modifiers::SHIFT, egui::Key::Enter)));
        let resp = ui.add(edit);
        let send = resp.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter) && !i.modifiers.shift);
        if send {
            let text = draft.trim().to_string();
            draft.clear();
            if !text.is_empty()
                && let Some(link) = &core.link
            {
                link.send(&meerora_protocol::Command::Say { agent: id.clone(), text });
            }
            resp.request_focus();
        }
    });
    if voice_w > 0.0 {
        let area =
            Rect::from_min_max(pos2(rect.max.x - 12.0 - button_w - voice_w, rect.min.y), pos2(rect.max.x - 12.0 - button_w, rect.max.y));
        super::voice::chat_controls(ui, core, &id, area, rect);
    }
    if running {
        let b = Rect::from_min_size(pos2(rect.max.x - 12.0 - button_w, rect.center().y - 16.0), vec2(button_w, 32.0));
        if button(ui, b, "Stop", ButtonStyle::Quiet).clicked()
            && let Some(link) = &core.link
        {
            link.send(&meerora_protocol::Command::Cancel { agent: id });
        }
    }
}

pub(super) fn placeholder(ui: &mut Ui, body: Rect, title: &str, text: &str) {
    let painter = ui.painter();
    let w = (body.width() - 64.0).min(420.0);
    painter.text(body.center() - vec2(0.0, 22.0), Align2::CENTER_CENTER, title, theme::ui_semibold(16.0), c([0xC9, 0xCE, 0xD9]));
    let galley = painter.layout(text.to_string(), theme::ui(14.0), c(theme::TEXT_2), w);
    let pos = pos2(body.center().x - galley.size().x / 2.0, body.center().y);
    painter.galley(pos, galley, c(theme::TEXT_2));
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, OnceLock};
    use std::time::{Duration, Instant};

    use egui::{Event, Key, Modifiers, PointerButton, Pos2, Rect, pos2, vec2};
    use meerora_term::Spawn;

    use super::*;
    use crate::assets::Assets;

    fn screen() -> Rect {
        Rect::from_min_size(Pos2::ZERO, vec2(1440.0, 900.0))
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

    /// One headless GPU for these tests, like the scene tests share one.
    fn gpu() -> Option<&'static meerora_scene::Gpu> {
        static GPU: OnceLock<Option<meerora_scene::Gpu>> = OnceLock::new();
        GPU.get_or_init(|| match meerora_scene::Gpu::headless() {
            Ok(gpu) => Some(gpu),
            Err(e) => {
                eprintln!("skipped: no headless GPU ({e:#})");
                None
            }
        })
        .as_ref()
    }

    /// The real app on a headless GPU with Wren's view open, in its own data
    /// directory with `apps` in Wren's agent.toml. Terminals run `/bin/sh`
    /// with their own HOME, so nothing reaches the user's files.
    struct Harness {
        gpu: &'static meerora_scene::Gpu,
        ctx: egui::Context,
        core: Core,
        data: PathBuf,
        time: f64,
    }

    impl Harness {
        fn new(name: &str, apps: &str) -> Option<Harness> {
            let gpu = gpu()?;
            let assets = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets");
            let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().subsec_nanos();
            let data = std::env::temp_dir().join(format!("meerora-instances-{name}-{}-{nanos}", std::process::id()));
            copy_dir(&assets.join("spaces"), &data.join("spaces"));
            copy_dir(&assets.join("agents/wren"), &data.join("agents/wren"));
            let toml = data.join("agents/wren/agent.toml");
            let text = std::fs::read_to_string(&toml).unwrap().replace("home = \"window\"", &format!("home = \"window\"\napps = {apps}"));
            std::fs::write(&toml, text).unwrap();
            let home = data.join("home");
            std::fs::create_dir_all(&home).unwrap();

            let mut core =
                Core::new(&gpu.device, meerora_scene::IMAGE_FORMAT, Assets::without_watcher(data.clone()), None, Arc::new(|| {}));
            core.load(&gpu.device, &gpu.queue);
            let home_env = home.display().to_string();
            core.terminal_spawn = Spawn {
                program: Some("/bin/sh".into()),
                env: ["HOME", "XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_CACHE_HOME", "XDG_STATE_HOME"]
                    .into_iter()
                    .map(|k| (k.to_string(), home_env.clone()))
                    .chain([("HISTFILE".to_string(), "/dev/null".to_string()), ("PS1".to_string(), "$ ".to_string())])
                    .collect(),
                ..Default::default()
            };
            core.ui.view = View::Agent("wren".into());
            let ctx = egui::Context::default();
            crate::theme::install(&ctx);
            Some(Harness { gpu, ctx, core, data, time: 0.0 })
        }

        /// One UI frame with these input events, as the window runs it.
        fn frame(&mut self, events: Vec<Event>) {
            self.time += 1.0 / 30.0;
            self.core.step(&self.gpu.device, &self.gpu.queue, 1.0 / 30.0);
            let raw = egui::RawInput { screen_rect: Some(screen()), time: Some(self.time), events, ..Default::default() };
            let core = &mut self.core;
            let out = self.ctx.run_ui(raw, |ui| core.build_ui(ui));
            self.core.apply_textures(&self.gpu.device, &self.gpu.queue, out.textures_delta);
        }

        fn click(&mut self, at: Pos2) {
            let button = |pressed| Event::PointerButton { pos: at, button: PointerButton::Primary, pressed, modifiers: Modifiers::NONE };
            self.frame(vec![Event::PointerMoved(at)]);
            self.frame(vec![button(true)]);
            self.frame(vec![button(false)]);
        }

        /// Runs frames until `done` or the timeout, and says whether it got there.
        fn until(&mut self, timeout: Duration, mut done: impl FnMut(&mut Core) -> bool) -> bool {
            let deadline = Instant::now() + timeout;
            while Instant::now() < deadline {
                self.frame(Vec::new());
                if done(&mut self.core) {
                    return true;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            false
        }

        fn rect_of(&mut self, tab: AppTab) -> Rect {
            let content = content_rect(screen());
            let apps = apps_of(&self.core, "wren");
            let rects = crate::ui::tiles::layout_for(&mut self.core, "wren", &apps, content).rects(content);
            rects.into_iter().find(|(t, _)| *t == tab).map(|(_, r)| r).unwrap_or_else(|| panic!("no tile for {tab}"))
        }

        fn open(&mut self, tab: AppTab) {
            open_app(&mut self.core, "wren", tab, content_rect(screen()));
            self.frame(Vec::new());
        }
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            // Chrome and the shells end first, so nothing writes into the data
            // directory, this test's own copy, while it goes.
            let idle = crate::ui::browser::Browsers::new(self.data.clone(), Arc::new(|| {}));
            drop(std::mem::replace(&mut self.core.browsers, idle));
            self.core.close_terminals("wren");
            let _ = std::fs::remove_dir_all(&self.data);
        }
    }

    /// The text on the screen of terminal `n` of Wren.
    fn screen_text(core: &mut Core, n: u32) -> String {
        let Some(tile) = core.terminal("wren", n) else { return String::new() };
        let snap = tile.term.snapshot();
        let cols = snap.size.cols as usize;
        snap.cells
            .chunks(cols)
            .map(|row| row.iter().map(|c| c.ch).collect::<String>().trim_end().to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn key(key: Key, modifiers: Modifiers, pressed: bool) -> Event {
        Event::Key { key, physical_key: Some(key), pressed, repeat: false, modifiers }
    }

    /// A click into the terminal tile gives it the keyboard, and Tab and
    /// Shift+Tab then reach the program on its PTY instead of moving egui's
    /// focus to the next widget. Also right in the frame after the click.
    #[test]
    fn tab_reaches_the_pty() {
        let Some(mut h) = Harness::new("tab", r#"["chat", "terminal"]"#) else { return };
        // Reads four raw bytes from the PTY and prints them in hex.
        h.core.terminal_spawn.args = vec!["-c".into(), "stty -icanon -echo; echo ready; head -c 4 | od -An -tx1; sleep 30".into()];
        assert!(h.until(Duration::from_secs(10), |core| screen_text(core, 1).contains("ready")), "the program started");
        let tile = h.rect_of(AppTab::Terminal(1));
        let body = pos2(tile.center().x, tile.min.y + 120.0);
        // Tab in the same batch of input as the click's release, the first frame with focus.
        h.frame(vec![Event::PointerMoved(body)]);
        h.frame(vec![Event::PointerButton { pos: body, button: PointerButton::Primary, pressed: true, modifiers: Modifiers::NONE }]);
        h.frame(vec![Event::PointerButton { pos: body, button: PointerButton::Primary, pressed: false, modifiers: Modifiers::NONE }]);
        h.frame(vec![key(Key::Tab, Modifiers::NONE, true), key(Key::Tab, Modifiers::NONE, false)]);
        let term_id = egui::Id::new(("term", "wren"));
        assert!(h.ctx.memory(|m| m.has_focus(term_id)), "the terminal keeps the keyboard after Tab");
        h.frame(Vec::new());
        h.frame(vec![key(Key::Tab, Modifiers::NONE, true), key(Key::Tab, Modifiers::NONE, false)]);
        h.frame(vec![key(Key::Tab, Modifiers::SHIFT, true), key(Key::Tab, Modifiers::SHIFT, false)]);
        let got = h.until(Duration::from_secs(10), |core| screen_text(core, 1).contains("09 09 1b 5b"));
        assert!(got, "the PTY got Tab, Tab and the start of Shift+Tab:\n{}", screen_text(&mut h.core, 1));
        assert!(h.ctx.memory(|m| m.has_focus(term_id)), "the terminal still has the keyboard");
        assert_eq!(h.core.ui.view, View::Agent("wren".into()));
    }

    /// A second terminal is its own shell in the agent's directory, opens
    /// beside the first, and closing it ends only its shell.
    #[test]
    fn a_second_terminal_opens_and_closes_alone() {
        let Some(mut h) = Harness::new("terminals", r#"["chat", "terminal"]"#) else { return };
        h.frame(Vec::new());
        assert_eq!(next_instance(&h.core, "wren", App::Terminal), AppTab::Terminal(2));
        h.open(AppTab::Terminal(2));
        assert_eq!(h.core.ui.tab, AppTab::Terminal(2), "the new terminal is the tab");
        assert_eq!(open_tabs(&h.core, "wren"), [AppTab::Chat, AppTab::Terminal(1), AppTab::Terminal(2)]);
        assert_eq!(super::super::tab_title(&h.core, AppTab::Terminal(2)), "Terminal 2");
        assert!(h.until(Duration::from_secs(10), |core| core.terminal_instances("wren") == [1, 2]), "two shells run");

        let dir = h.core.agent_dir("wren").unwrap().path.display().to_string();
        h.core.terminal("wren", 2).unwrap().term.write(b"pwd; echo pid=$$\r");
        h.core.terminal("wren", 1).unwrap().term.write(b"echo pid=$$\r");
        let both = h.until(Duration::from_secs(10), |core| {
            screen_text(core, 2).contains(&dir) && screen_text(core, 2).contains("pid=") && screen_text(core, 1).contains("pid=")
        });
        assert!(both, "terminal 2 runs in {dir}:\n{}", screen_text(&mut h.core, 2));
        let pid = |text: String| text.lines().find_map(|l| l.strip_prefix("pid=").map(str::to_string));
        assert_ne!(pid(screen_text(&mut h.core, 1)), pid(screen_text(&mut h.core, 2)), "each terminal is its own shell");

        // Another app opens things in the instance you used last.
        h.core.ui.tab = AppTab::Chat;
        assert_eq!(target(&h.core, "wren", App::Terminal), AppTab::Terminal(2));

        // The cross on the head of terminal 2 closes it.
        let tile = h.rect_of(AppTab::Terminal(2));
        h.core.ui.tab = AppTab::Terminal(2);
        h.click(pos2(tile.max.x - 20.0, tile.min.y + 19.0));
        assert_eq!(open_tabs(&h.core, "wren"), [AppTab::Chat, AppTab::Terminal(1)]);
        assert_eq!(h.core.terminal_instances("wren"), [1], "only the second shell ended");
        assert_eq!(h.core.ui.tab, AppTab::Terminal(1), "the other terminal becomes the tab");
        assert!(h.core.terminal("wren", 1).unwrap().term.is_alive());
        assert_eq!(next_instance(&h.core, "wren", App::Terminal), AppTab::Terminal(2), "the number comes back");
        let saved = std::fs::read_to_string(h.data.join("layouts.json")).unwrap();
        assert!(saved.contains(r#""terminal""#) && !saved.contains(r#"{"terminal":2}"#), "{saved}");

        // The "+" of the shell strip opens a second shell as a tab of the same tile.
        let tile = h.rect_of(AppTab::Terminal(1));
        let tiles = open_tabs(&h.core, "wren").len();
        let title_w = h.ctx.fonts_mut(|f| f.layout_no_wrap("Terminal".into(), theme::ui_semibold(14.5), egui::Color32::WHITE).size().x);
        let strip_x = tile.min.x + 16.0 + title_w + 12.0;
        h.click(pos2(strip_x + 12.0, tile.min.y + 19.0));
        assert_eq!(h.core.ui.tab, AppTab::Terminal(2), "the new shell is the tab");
        assert_eq!(open_tabs(&h.core, "wren").len(), tiles + 1);
        assert_eq!(h.rect_of(AppTab::Terminal(2)), tile, "it shows in the same tile");
        let group = |h: &Harness| h.core.ui.tiles.as_ref().unwrap()["wren"].group(AppTab::Terminal(1));
        assert_eq!(group(&h), [AppTab::Terminal(2), AppTab::Terminal(1)]);
        // Its strip has a tab per shell, the title is now "Terminal 2". Tab 1 shows the first shell again.
        let title_w = h.ctx.fonts_mut(|f| f.layout_no_wrap("Terminal 2".into(), theme::ui_semibold(14.5), egui::Color32::WHITE).size().x);
        let strip_x = tile.min.x + 16.0 + title_w + 12.0;
        h.click(pos2(strip_x + 13.0, tile.min.y + 19.0));
        assert_eq!(h.core.ui.tab, AppTab::Terminal(1));
        assert_eq!(h.rect_of(AppTab::Terminal(1)), tile);
        assert!(h.until(Duration::from_secs(10), |core| core.terminal_instances("wren") == [1, 2]), "both shells run");
        // Closing the shown shell shows the other one in the tile.
        h.click(pos2(tile.max.x - 20.0, tile.min.y + 19.0));
        assert_eq!(h.core.ui.tab, AppTab::Terminal(2));
        assert_eq!(h.rect_of(AppTab::Terminal(2)), tile);
        assert_eq!(h.core.terminal_instances("wren"), [2]);
    }

    /// A second browser is a second page in the agent's one Chrome. Each
    /// shows its own page, and closing the second leaves the first running.
    #[test]
    fn a_second_browser_is_a_page_of_the_same_chrome() {
        if meerora_browser::find_chrome(None).is_none() {
            eprintln!("skipped: Chrome is not installed");
            return;
        }
        let Some(mut h) = Harness::new("browsers", r#"["chat", "browser"]"#) else { return };
        crate::ui::browser::navigate(&mut h.core, "wren", 1, "data:text/html,<title>one</title><h1>one</h1>");
        h.frame(Vec::new());
        h.open(next_instance(&h.core, "wren", App::Browser));
        crate::ui::browser::navigate(&mut h.core, "wren", 2, "data:text/html,<title>two</title><h1>two</h1>");
        assert_eq!(h.core.ui.tab, AppTab::Browser(2));
        let title = |core: &Core, n| core.browsers.page("wren", n).map(|p| p.title).unwrap_or_default();
        let both = h.until(Duration::from_secs(45), |core| title(&*core, 1) == "one" && title(&*core, 2) == "two");
        assert!(both, "each browser shows its page: {:?} {:?}", h.core.browsers.page("wren", 1), h.core.browsers.page("wren", 2));
        assert_eq!(h.core.browsers.instances("wren"), [1, 2]);
        let profile = meerora_browser::profile_dir(&h.data, "wren");
        assert!(profile.is_dir(), "one profile for the agent");

        let tile = h.rect_of(AppTab::Browser(2));
        h.click(pos2(tile.max.x - 20.0, tile.min.y + 19.0));
        assert_eq!(open_tabs(&h.core, "wren"), [AppTab::Chat, AppTab::Browser(1)]);
        assert_eq!(h.core.browsers.instances("wren"), [1], "only the second page closed");
        let frames = h.core.browsers.page("wren", 1).map_or(0, |p| p.frames);
        crate::ui::browser::navigate(&mut h.core, "wren", 1, "data:text/html,<title>still</title>");
        let alive = h.until(Duration::from_secs(20), |core| title(&*core, 1) == "still");
        assert!(alive, "the first page still runs: {:?}", h.core.browsers.page("wren", 1));
        assert!(h.core.browsers.page("wren", 1).map_or(0, |p| p.frames) >= frames);
    }
}
