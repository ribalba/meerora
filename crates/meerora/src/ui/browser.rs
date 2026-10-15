//! The browser tile of the agent view (D11, D12, D13): headless Chrome per
//! agent, driven over CDP by meerora-browser and drawn from its screencast
//! frames. Your mouse, wheel and keys go to the page. When the agent logs
//! `browser.open` or `browser.search`, the tile goes there and the head
//! says who opened it, with Take over.
//!
//! Chrome starts the first time a browser tile of the agent is shown, one
//! per agent, with its own profile in `<data dir>/browser/<agent id>/`.
//! Every Browser instance of the agent (`AppTab::Browser(n)`) is its own
//! page in that Chrome, another CDP target, because two Chromes cannot share
//! a profile. Closing an instance closes its page, and Chrome ends with its
//! last page, when the agent is removed and when the app exits. The agent's
//! `browser.open` and `browser.search` go to browser 1.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use egui::{Align2, Color32, CornerRadius, Event, EventFilter, Id, Pos2, Rect, Sense, Stroke, StrokeKind, Ui, pos2, vec2};
use meerora_browser::{Browser, Button, Key, Mods, MouseEvent, MouseKind, Options, Size, Status};

use super::{AppTab, ButtonStyle, button, fit, pill};
use crate::engine::{Core, Wake};
use crate::theme::{self, c, ca};

const NAV_H: f32 = 40.0;

/// Browser pages of all agents, by agent and instance number.
pub struct Browsers {
    tiles: HashMap<(String, u32), BrowserTile>,
    data_dir: PathBuf,
    wake: Wake,
}

#[derive(Default)]
struct BrowserTile {
    browser: Option<Browser>,
    texture: Option<egui::TextureHandle>,
    /// Size of the shown frame in points.
    frame_size: egui::Vec2,
    url_text: String,
    /// The page URL the field last followed.
    shown_url: String,
    /// The viewport last sent to Chrome.
    size: Option<(Size, f32)>,
    /// Time of the newest action already handled.
    seen: Option<f64>,
    opened_by: Option<f64>,
    taken_over: bool,
    /// A navigation was asked for, so the snapshot waits for its load.
    navigated: bool,
    input: Input,
    /// The URL to load once Chrome runs again after a restart.
    restart_url: Option<String>,
}

/// Pointer state between frames.
#[derive(Default)]
struct Input {
    last_mouse: Option<Pos2>,
    held: Button,
    /// Last press, for double clicks: when, where, how many.
    last_press: Option<(Instant, Pos2, u32)>,
}

impl Browsers {
    pub fn new(data_dir: PathBuf, wake: Wake) -> Self {
        Self { tiles: HashMap::new(), data_dir, wake }
    }

    /// Closes the agent's Chrome with all its pages, without waiting (the
    /// agent was removed).
    pub fn close(&mut self, agent: &str) {
        self.tiles.retain(|(a, _), _| a != agent);
    }

    /// Closes the page of browser `n` of the agent, without waiting. The
    /// other pages stay, and Chrome ends with its last page.
    pub fn close_instance(&mut self, agent: &str, n: u32) {
        self.tiles.remove(&(agent.to_string(), n));
    }

    /// The numbers of the agent's browsers that have a page or wait for one, lowest first.
    #[cfg(test)]
    pub fn instances(&self, agent: &str) -> Vec<u32> {
        let mut out: Vec<u32> = self.tiles.iter().filter(|((a, _), t)| a == agent && t.browser.is_some()).map(|((_, n), _)| *n).collect();
        out.sort_unstable();
        out
    }

    /// What browser `n` of the agent shows, while it has a page.
    #[cfg(test)]
    pub fn page(&self, agent: &str, n: u32) -> Option<meerora_browser::Page> {
        self.tiles.get(&(agent.to_string(), n)).and_then(|t| t.browser.as_ref()).map(Browser::page)
    }

    /// Waits until every started browser shows its page or failed, for the
    /// snapshot mode.
    pub fn wait_settled(&self, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        let settled = |t: &BrowserTile| {
            let Some(b) = &t.browser else { return true };
            match b.status() {
                Status::Starting => false,
                Status::Failed(_) | Status::Exited(_) => true,
                Status::Running => {
                    let page = b.page();
                    page.frames > 0 && !page.loading && (!t.navigated || page.loads > 0)
                }
            }
        };
        while Instant::now() < deadline && !self.tiles.values().all(settled) {
            std::thread::sleep(Duration::from_millis(50));
        }
        if !self.tiles.is_empty() {
            // Chrome paints once more after the load event.
            std::thread::sleep(Duration::from_millis(400));
        }
    }
}

impl Drop for Browsers {
    /// Closes every Chrome and waits a moment for them to exit.
    fn drop(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(3);
        let browsers: Vec<Browser> = self.tiles.drain().filter_map(|(_, t)| t.browser).collect();
        for b in &browsers {
            b.close();
        }
        for b in browsers {
            let left = deadline.saturating_duration_since(Instant::now());
            b.shutdown(left);
        }
    }
}

/// Navigates a browser of `agent` to `url` for you and opens it, the one
/// you used last when there are several (`agent::target`). Other apps such
/// as the Viewer use this. A page that has not started yet loads the URL
/// when it starts.
pub(crate) fn open_url(core: &mut Core, agent: &str, url: &str) {
    let tab = super::agent::target(core, agent, meerora_protocol::App::Browser);
    navigate(core, agent, tab.instance(), url);
    super::agent::open_later(core, tab);
}

/// Navigates browser `n` of `agent` to `url` for you, without opening its
/// tile. A page that has not started yet loads the URL when it starts.
pub(crate) fn navigate(core: &mut Core, agent: &str, n: u32, url: &str) {
    let newest = core
        .world
        .agent(agent)
        .and_then(|a| a.actions.iter().filter(|e| e.tool == "browser.open" || e.tool == "browser.search").map(|e| e.at).next_back());
    let t = core.browsers.tiles.entry((agent.to_string(), n)).or_default();
    // Pages the agent opened before do not replace this one when the tile first shows.
    if t.seen.is_none() {
        t.seen = Some(newest.unwrap_or(f64::NEG_INFINITY));
    }
    match &t.browser {
        Some(b) => b.navigate(url),
        None => t.restart_url = Some(url.to_string()),
    }
    t.url_text = url.to_string();
    t.navigated = true;
    t.opened_by = None;
}

/// The page a logged `browser.open` or `browser.search` opens, from the
/// action's full target. `None` when the agent may not open it.
fn page_of(tool: &str, target: &str, agent_dir: Option<&std::path::Path>) -> Option<String> {
    if tool == "browser.search" {
        Some(meerora_browser::url::search_url(target))
    } else {
        meerora_browser::url::for_agent(target, agent_dir)
    }
}

/// Whether a page of this Chrome can open another page in it.
fn alive(b: &Browser) -> bool {
    !b.is_finished() && matches!(b.status(), Status::Starting | Status::Running)
}

/// Draws browser `n` of agent `index`. Returns true while the page has
/// keyboard focus, so Esc goes to the page. Only browser 1 follows the
/// pages the agent opens.
pub fn tile(ui: &mut Ui, core: &mut Core, index: usize, n: u32, head: Rect, body: Rect) -> bool {
    let a = &core.world.agents[index];
    let id = a.id.clone();
    let key = super::instance_key(&id, n);
    let name = a.name.clone();
    let now = core.world.time;
    let ppp = ui.ctx().pixels_per_point();
    // What the agent does in this page through the app tools, for the head (D12, D30).
    let doing = crate::agent_apps::doing(core, &id, AppTab::Browser(n));
    let actions: Vec<(f64, String, String)> = a
        .actions
        .iter()
        .filter(|e| n == 1 && (e.tool == "browser.open" || e.tool == "browser.search"))
        // The full URL or query, since the summary of a long one is cut.
        .map(|e| (e.at, e.tool.clone(), e.target().to_string()))
        .collect();
    let profile = meerora_browser::profile_dir(&core.browsers.data_dir, &id);
    // An agent may open files of its own directory, no others (`url::for_agent`).
    let agent_dir = core.agent_dir(&id).map(|d| d.path.clone());
    let wake = core.browsers.wake.clone();

    let nav = Rect::from_min_size(body.min, vec2(body.width(), NAV_H));
    let page_r = Rect::from_min_max(pos2(body.min.x, nav.max.y), body.max);
    let size = Size { width: page_r.width().floor().max(16.0) as u32, height: page_r.height().floor().max(16.0) as u32 };

    // Start Chrome the first time a browser tile of the agent shows (D13).
    // Further instances are further pages of the same Chrome.
    let starting = core.browsers.tiles.get(&(id.clone(), n)).is_none_or(|t| t.browser.is_none());
    let sibling = if starting {
        let url = core.browsers.tiles.get(&(id.clone(), n)).and_then(|t| t.restart_url.clone());
        core.browsers
            .tiles
            .iter()
            .filter(|((agent, k), _)| *agent == id && *k != n)
            .find_map(|(_, t)| t.browser.as_ref().filter(|b| alive(b)))
            .map(|b| b.open_page(size, ppp, url.as_deref()))
    } else {
        None
    };
    let t = core.browsers.tiles.entry((id.clone(), n)).or_default();
    if t.browser.is_none() {
        let b = match sibling {
            Some(page) => {
                if t.restart_url.take().is_some() {
                    t.navigated = true;
                }
                page
            }
            None => {
                let mut options = Options::new(profile);
                options.size = size;
                options.scale = ppp;
                let b = Browser::start(options);
                if let Some(url) = t.restart_url.take() {
                    b.navigate(&url);
                    t.navigated = true;
                }
                b
            }
        };
        b.set_wakeup(Box::new(move || wake()));
        t.size = Some((size, ppp));
        t.browser = Some(b);
    }

    // Pages the agent opened (D12). The first time only the newest counts.
    let first = t.seen.is_none();
    let new: Vec<&(f64, String, String)> = actions.iter().filter(|(at, _, _)| t.seen.is_none_or(|s| *at > s)).collect();
    let todo: Vec<&(f64, String, String)> = if first { new.last().copied().into_iter().collect() } else { new };
    for (at, tool, target) in todo {
        if t.taken_over {
            continue;
        }
        let url = page_of(tool, target, agent_dir.as_deref());
        if let (Some(url), Some(b)) = (url, &t.browser) {
            b.navigate(&url);
            t.url_text = url;
            t.navigated = true;
            t.opened_by = Some(*at);
        }
    }
    if let Some((at, _, _)) = actions.last() {
        t.seen = Some(t.seen.map_or(*at, |s| s.max(*at)));
    } else if first {
        t.seen = Some(f64::NEG_INFINITY);
    }

    let Some(b) = t.browser.as_ref() else { return false };
    let status = b.status();
    let page = b.page();
    if let Some(frame) = b.take_frame() {
        match image::load_from_memory_with_format(&frame.jpeg, image::ImageFormat::Jpeg) {
            Ok(img) => {
                let rgba = img.to_rgba8();
                let px = [rgba.width() as usize, rgba.height() as usize];
                let color = egui::ColorImage::from_rgba_unmultiplied(px, rgba.as_raw());
                match &mut t.texture {
                    Some(tex) => tex.set(color, egui::TextureOptions::LINEAR),
                    None => t.texture = Some(ui.ctx().load_texture(format!("browser-{key}"), color, egui::TextureOptions::LINEAR)),
                }
                t.frame_size = if frame.css_width > 0.0 {
                    vec2(frame.css_width, frame.css_height)
                } else {
                    vec2(px[0] as f32 / ppp, px[1] as f32 / ppp)
                };
            }
            Err(e) => tracing::debug!("cannot decode a browser frame: {e}"),
        }
    }
    if status == Status::Running && t.size != Some((size, ppp)) {
        b.resize(size, ppp);
        t.size = Some((size, ppp));
    }

    // Head: who controls the window, with Take over (D12).
    let agent_controls = !t.taken_over && (t.opened_by.is_some() || doing.is_some());
    let (chip, color) = if t.taken_over {
        ("YOU · TAKEN OVER".to_string(), theme::NEEDS_YOU)
    } else if let Some(verb) = doing {
        (format!("{} IS {}", name.to_uppercase(), verb.to_uppercase()), theme::WORKING)
    } else if let Some(at) = t.opened_by {
        (format!("OPENED BY {} · {}", name.to_uppercase(), ago(now - at)), theme::WORKING)
    } else {
        ("YOU".to_string(), [0xB9, 0xBF, 0xCC])
    };
    let mut right = head.max.x - 12.0;
    if t.taken_over || agent_controls {
        let r = Rect::from_min_size(pos2(right - 92.0, head.center().y - 13.0), vec2(92.0, 26.0));
        let text = if t.taken_over { "Hand back" } else { "Take over" };
        if button(ui, r, text, ButtonStyle::Quiet).clicked() {
            t.taken_over = !t.taken_over;
            if t.taken_over {
                t.opened_by = None;
            }
        }
        right = r.min.x - 10.0;
    }
    let chip_r = pill(ui.painter(), pos2(right, head.center().y), &chip, color);
    if !page.title.is_empty() {
        let painter = ui.painter();
        let x = head.min.x + 84.0;
        let title = fit(painter, &page.title, theme::ui(13.0), chip_r.min.x - x - 12.0);
        painter.text(pos2(x, head.center().y), Align2::LEFT_CENTER, title, theme::ui(13.0), c(theme::TEXT_2));
    }

    // Navigation row: back, forward, reload and the URL field.
    let painter = ui.painter_at(nav);
    painter.rect_filled(nav, CornerRadius::ZERO, c(theme::PANEL));
    super::hline(&painter, nav.max.y - 0.5, nav.min.x, nav.max.x);
    let mut user_navigated = false;
    let icon_r = |i: f32| Rect::from_min_size(pos2(nav.min.x + 8.0 + i * 34.0, nav.center().y - 16.0), vec2(32.0, 32.0));
    let running = status == Status::Running;
    for (i, (icon, enabled)) in
        [(Icon::Back, page.can_go_back), (Icon::Forward, page.can_go_forward), (Icon::Reload, running)].into_iter().enumerate()
    {
        let r = icon_r(i as f32);
        let resp = ui.interact(r, Id::new(("browser-nav", &key, i)), if enabled && running { Sense::click() } else { Sense::hover() });
        let painter = ui.painter_at(nav);
        if resp.hovered() && enabled {
            painter.rect_filled(r, CornerRadius::same(8), c([0x1A, 0x1F, 0x2A]));
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        draw_icon(&painter, r.center(), icon, if enabled { c(theme::TEXT_2) } else { c([0x3A, 0x40, 0x4E]) });
        if resp.clicked() {
            match icon {
                Icon::Back => b.back(),
                Icon::Forward => b.forward(),
                Icon::Reload => b.reload(),
            }
            user_navigated = true;
        }
    }
    let field = Rect::from_min_max(pos2(icon_r(3.0).min.x + 4.0, nav.center().y - 14.0), pos2(nav.max.x - 10.0, nav.center().y + 14.0));
    let field_id = Id::new(("browser-url", &key));
    let editing = ui.memory(|m| m.has_focus(field_id));
    if page.url != t.shown_url {
        // The page moved on (a link, a redirect, back): the field follows unless you are typing.
        t.shown_url = page.url.clone();
        if !editing && page.url != "about:blank" {
            t.url_text = page.url.clone();
        }
    }
    let painter = ui.painter_at(nav);
    painter.rect(
        field,
        CornerRadius::same(8),
        c(theme::BG),
        Stroke::new(1.0, c(if editing { [0x3A, 0x42, 0x52] } else { theme::BORDER })),
        StrokeKind::Inside,
    );
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(field.shrink2(vec2(10.0, 5.0))));
    let edit = egui::TextEdit::singleline(&mut t.url_text)
        .id(field_id)
        .font(theme::mono(12.0))
        .frame(egui::Frame::NONE)
        .margin(egui::Margin::ZERO)
        .text_color(c([0xC9, 0xCE, 0xD9]))
        .hint_text(egui::RichText::new("Search or type a URL").color(c([0x6B, 0x72, 0x82])))
        .desired_width(field.width() - 20.0);
    let resp = child.add(edit);
    if resp.lost_focus()
        && ui.input(|i| i.key_pressed(egui::Key::Enter))
        && let Some(url) = meerora_browser::url::for_input(&t.url_text)
    {
        b.navigate(&url);
        t.url_text = url;
        t.navigated = true;
        user_navigated = true;
    }
    if page.loading && running {
        // A thin moving bar while the page loads.
        let w = nav.width() * 0.25;
        let phase = (now * 0.8).fract() as f32;
        let x0 = nav.min.x + (nav.width() + w) * phase - w;
        let bar = Rect::from_min_max(pos2(x0.max(nav.min.x), nav.max.y - 2.0), pos2((x0 + w).min(nav.max.x), nav.max.y));
        ui.painter_at(nav).rect_filled(bar, CornerRadius::ZERO, c(theme::NEEDS_YOU));
        ui.ctx().request_repaint();
    }

    // The page.
    // The tile's bottom corners are round, so the page is too.
    let page_corners = CornerRadius { nw: 0, ne: 0, sw: 13, se: 13 };
    let painter = ui.painter_at(page_r);
    painter.rect_filled(page_r, page_corners, c(theme::TERMINAL_BG));
    let mut focused = false;
    match &status {
        Status::Failed(msg) | Status::Exited(msg) => {
            let title = if matches!(status, Status::Failed(_)) { "The browser could not start" } else { "Chrome stopped" };
            message(ui, page_r, title, msg);
            let r = Rect::from_center_size(page_r.center() + vec2(0.0, 70.0), vec2(120.0, 34.0));
            if button(ui, r, "Start again", ButtonStyle::Quiet).clicked() {
                t.restart_url = Some(page.url.clone()).filter(|u| !u.is_empty() && u != "about:blank");
                t.browser = None;
                t.texture = None;
                t.size = None;
            }
        }
        Status::Starting if t.texture.is_none() => message(ui, page_r, "Starting Chrome", "The page shows here in a moment."),
        _ => {
            if let Some(tex) = &t.texture {
                let shown = Rect::from_min_size(page_r.min, t.frame_size);
                let uv = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
                let corners = if (shown.max - page_r.max).length() < 2.0 { page_corners } else { CornerRadius::ZERO };
                painter.add(egui::epaint::RectShape::filled(shown, corners, Color32::WHITE).with_texture(tex.id(), uv));
            }
            if page.crashed {
                painter.rect_filled(page_r, page_corners, ca(theme::BG, 210));
                message(ui, page_r, "The page crashed", "Reload to try again.");
            }
            if let Some(b) = t.browser.as_ref() {
                let typed;
                (focused, typed) = forward_input(ui, b, &key, page_r, &mut t.input);
                user_navigated |= typed;
            }
        }
    }
    if user_navigated {
        // You drive the page now. Take over keeps the agent out until you hand back.
        t.opened_by = None;
    }
    if agent_controls {
        let frame = Rect::from_min_max(head.min, pos2(body.max.x + 1.0, body.max.y + 1.0));
        ui.painter().rect_stroke(frame, CornerRadius::same(14), Stroke::new(1.0, ca(theme::WORKING, 110)), StrokeKind::Inside);
    }
    if ui.rect_contains_pointer(body) && ui.input(|i| i.pointer.any_pressed()) {
        core.ui.tab = AppTab::Browser(n);
    }
    focused
}

/// Mouse, wheel and keys for the page. Returns whether the page has
/// keyboard focus and whether you typed into it. `key` is the instance key.
fn forward_input(ui: &mut Ui, b: &Browser, key: &str, page_r: Rect, t: &mut Input) -> (bool, bool) {
    let page_id = Id::new(("browser-page", key));
    let resp = ui.interact(page_r, page_id, Sense::click_and_drag());
    if resp.is_pointer_button_down_on() || resp.clicked() {
        resp.request_focus();
    }
    let focused = resp.has_focus();
    if focused {
        ui.memory_mut(|m| {
            m.set_focus_lock_filter(page_id, EventFilter { tab: true, horizontal_arrows: true, vertical_arrows: true, escape: true })
        });
    }
    let to_page = |p: Pos2| p - page_r.min;
    let events = ui.input(|i| i.events.clone());
    let hovered = resp.hovered() || resp.dragged();
    let mut typed = false;
    for event in events {
        match event {
            Event::PointerMoved(p) if hovered || t.held != Button::None => {
                if t.last_mouse != Some(p) {
                    t.last_mouse = Some(p);
                    let v = to_page(p);
                    let mut m = MouseEvent::new(MouseKind::Move, v.x, v.y);
                    m.button = t.held;
                    b.mouse(m);
                }
            }
            Event::PointerButton { pos, button, pressed, modifiers } if page_r.contains(pos) || (!pressed && t.held != Button::None) => {
                let btn = match button {
                    egui::PointerButton::Primary => Button::Left,
                    egui::PointerButton::Secondary => Button::Right,
                    egui::PointerButton::Middle => Button::Middle,
                    _ => continue,
                };
                let v = to_page(pos);
                let clicks = if pressed {
                    let n = match t.last_press {
                        Some((at, p, n)) if at.elapsed() < Duration::from_millis(450) && p.distance(pos) < 5.0 => (n % 3) + 1,
                        _ => 1,
                    };
                    t.last_press = Some((Instant::now(), pos, n));
                    n
                } else {
                    t.last_press.map_or(1, |(_, _, n)| n)
                };
                let mut m = MouseEvent::new(if pressed { MouseKind::Down } else { MouseKind::Up }, v.x, v.y);
                m.button = btn;
                m.clicks = clicks;
                m.mods = mods(modifiers);
                b.mouse(m);
                t.held = if pressed { btn } else { Button::None };
            }
            Event::MouseWheel { unit, delta, modifiers, .. } if hovered => {
                let scale = match unit {
                    egui::MouseWheelUnit::Point => 1.0,
                    egui::MouseWheelUnit::Line => 40.0,
                    egui::MouseWheelUnit::Page => page_r.height(),
                };
                let p = t.last_mouse.unwrap_or(page_r.center());
                let v = to_page(p);
                let mut m = MouseEvent::new(MouseKind::Wheel, v.x, v.y);
                m.dx = -delta.x * scale;
                m.dy = -delta.y * scale;
                m.mods = mods(modifiers);
                b.mouse(m);
            }
            Event::Text(text) if focused => {
                b.insert_text(&text);
                typed = true;
            }
            Event::Paste(text) if focused => {
                b.insert_text(&text);
                typed = true;
            }
            Event::Copy if focused => b.key(Key::Char('c'), Mods { ctrl: true, ..Default::default() }),
            Event::Cut if focused => b.key(Key::Char('x'), Mods { ctrl: true, ..Default::default() }),
            Event::Key { key, pressed: true, modifiers, .. } if focused => {
                let m = mods(modifiers);
                if let Some(k) = map_key(key, m) {
                    b.key(k, m);
                    typed |= matches!(k, Key::Enter | Key::Backspace | Key::Delete);
                }
            }
            _ => {}
        }
    }
    if hovered {
        ui.ctx().set_cursor_icon(egui::CursorIcon::Default);
    }
    (focused, typed)
}

fn mods(m: egui::Modifiers) -> Mods {
    Mods { alt: m.alt, ctrl: m.ctrl, meta: m.mac_cmd, shift: m.shift }
}

fn map_key(key: egui::Key, m: Mods) -> Option<Key> {
    use egui::Key as E;
    Some(match key {
        E::Enter => Key::Enter,
        E::Backspace => Key::Backspace,
        E::Tab => Key::Tab,
        E::Escape => Key::Escape,
        E::ArrowLeft => Key::Left,
        E::ArrowRight => Key::Right,
        E::ArrowUp => Key::Up,
        E::ArrowDown => Key::Down,
        E::Home => Key::Home,
        E::End => Key::End,
        E::PageUp => Key::PageUp,
        E::PageDown => Key::PageDown,
        E::Insert => Key::Insert,
        E::Delete => Key::Delete,
        E::F1 => Key::F(1),
        E::F2 => Key::F(2),
        E::F3 => Key::F(3),
        E::F4 => Key::F(4),
        E::F5 => Key::F(5),
        E::F6 => Key::F(6),
        E::F7 => Key::F(7),
        E::F8 => Key::F(8),
        E::F9 => Key::F(9),
        E::F10 => Key::F(10),
        E::F11 => Key::F(11),
        E::F12 => Key::F(12),
        // Plain characters and Space arrive as text. Only shortcuts are keys.
        _ if m.ctrl || m.alt || m.meta => {
            let mut chars = key.name().chars();
            match (chars.next(), chars.next()) {
                (Some(ch), None) if ch.is_ascii_alphanumeric() => Key::Char(ch.to_ascii_lowercase()),
                _ if key == E::Space => Key::Space,
                _ => return None,
            }
        }
        _ => return None,
    })
}

// Agent and app standard (D30, agent_apps.rs).

/// The viewport of a page an agent starts before its tile showed.
const AGENT_VIEWPORT: Size = Size { width: 1000, height: 700 };

/// A handle on browser `n` of `agent` for an agent's app tools, which work
/// on another thread. Chrome or the page starts when it does not run yet,
/// as when its tile first shows, at the size the tile last had.
pub(crate) fn agent_page(core: &mut Core, agent: &str, n: u32) -> Result<meerora_browser::Remote, String> {
    let key = (agent.to_string(), n);
    if let Some(b) = core.browsers.tiles.get(&key).and_then(|t| t.browser.as_ref()).filter(|b| alive(b)) {
        return Ok(b.remote());
    }
    let profile = meerora_browser::profile_dir(&core.browsers.data_dir, agent);
    let wake = core.browsers.wake.clone();
    let (size, scale) = core.browsers.tiles.get(&key).and_then(|t| t.size).unwrap_or((AGENT_VIEWPORT, 1.0));
    let url = core.browsers.tiles.get(&key).and_then(|t| t.restart_url.clone());
    let sibling = core
        .browsers
        .tiles
        .iter()
        .filter(|((a, k), _)| a == agent && *k != n)
        .find_map(|(_, t)| t.browser.as_ref().filter(|b| alive(b)))
        .map(|b| b.open_page(size, scale, url.as_deref()));
    let t = core.browsers.tiles.entry(key).or_default();
    let b = match sibling {
        Some(page) => page,
        None => {
            let mut options = Options::new(profile);
            options.size = size;
            options.scale = scale;
            let b = Browser::start(options);
            if let Some(url) = &url {
                b.navigate(url);
            }
            b
        }
    };
    if t.restart_url.take().is_some() {
        t.navigated = true;
    }
    b.set_wakeup(Box::new(move || wake()));
    let remote = b.remote();
    t.size = Some((size, scale));
    t.texture = None;
    t.browser = Some(b);
    Ok(remote)
}

/// The agent drives browser `n` now: its head says so, with Take over (D12).
pub(crate) fn agent_drives(core: &mut Core, agent: &str, n: u32) {
    let now = core.world.time;
    let t = core.browsers.tiles.entry((agent.to_string(), n)).or_default();
    t.opened_by = Some(now);
    t.navigated = true;
    if t.seen.is_none() {
        t.seen = Some(f64::NEG_INFINITY);
    }
}

/// Whether you took browser `n` of `agent` over (D12).
pub(crate) fn taken_over(core: &Core, agent: &str, n: u32) -> bool {
    core.browsers.tiles.get(&(agent.to_string(), n)).is_some_and(|t| t.taken_over)
}

/// Takes browser `n` of `agent` over, as the button on its head does.
#[cfg(test)]
pub(crate) fn tests_take_over(core: &mut Core, agent: &str, n: u32) {
    core.browsers.tiles.entry((agent.to_string(), n)).or_default().taken_over = true;
}

/// A page an act of the agent opened is logged as `browser.open` at `at`.
/// Browser 1 follows such lines, so it takes this one as seen.
pub(crate) fn mark_seen(core: &mut Core, agent: &str, at: f64) {
    let t = core.browsers.tiles.entry((agent.to_string(), 1)).or_default();
    t.seen = Some(t.seen.map_or(at, |s| s.max(at)));
}

fn ago(seconds: f64) -> String {
    let s = seconds.max(0.0) as u64;
    match s {
        0..60 => format!("{s} S AGO"),
        60..3600 => format!("{} MIN AGO", s / 60),
        _ => format!("{} H AGO", s / 3600),
    }
}

fn message(ui: &mut Ui, r: Rect, title: &str, text: &str) {
    let painter = ui.painter_at(r);
    painter.text(r.center() - vec2(0.0, 22.0), Align2::CENTER_CENTER, title, theme::ui_semibold(16.0), c([0xC9, 0xCE, 0xD9]));
    let galley = painter.layout(text.to_string(), theme::ui(13.5), c(theme::TEXT_2), (r.width() - 64.0).clamp(80.0, 440.0));
    painter.galley(pos2(r.center().x - galley.size().x / 2.0, r.center().y), galley, c(theme::TEXT_2));
}

#[derive(Clone, Copy)]
enum Icon {
    Back,
    Forward,
    Reload,
}

fn draw_icon(painter: &egui::Painter, center: Pos2, icon: Icon, color: Color32) {
    let s = Stroke::new(1.8, color);
    match icon {
        Icon::Back => {
            painter.line_segment([center + vec2(3.0, -6.0), center + vec2(-3.0, 0.0)], s);
            painter.line_segment([center + vec2(-3.0, 0.0), center + vec2(3.0, 6.0)], s);
        }
        Icon::Forward => {
            painter.line_segment([center + vec2(-3.0, -6.0), center + vec2(3.0, 0.0)], s);
            painter.line_segment([center + vec2(3.0, 0.0), center + vec2(-3.0, 6.0)], s);
        }
        Icon::Reload => {
            let r = 6.5;
            let points: Vec<Pos2> = (0..=24)
                .map(|k| {
                    let a = 0.35 + k as f32 / 24.0 * std::f32::consts::TAU * 0.8;
                    center + vec2(a.cos() * r, a.sin() * r)
                })
                .collect();
            let end = *points.last().unwrap_or(&center);
            painter.add(egui::Shape::line(points, s));
            painter.line_segment([end, end + vec2(0.5, -4.5)], s);
            painter.line_segment([end, end + vec2(4.5, 0.5)], s);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::ActionEntry;

    #[test]
    fn logged_pages_open_from_the_full_target() {
        let url = format!("https://example.org/{}?q=1", "deep/".repeat(30));
        let cut: String = url.chars().take(79).chain(['…']).collect();
        let open = ActionEntry { at: 1.0, tool: "browser.open".into(), summary: cut, target: Some(url.clone()) };
        assert_eq!(page_of(&open.tool, open.target(), None), Some(url), "not the cut summary");
        let old = ActionEntry { at: 1.0, tool: "browser.open".into(), summary: "https://example.org".into(), target: None };
        assert_eq!(page_of(&old.tool, old.target(), None).as_deref(), Some("https://example.org"), "an older daemon sends no target");
        let query = "rust ".repeat(30);
        let search = ActionEntry { at: 1.0, tool: "browser.search".into(), summary: "rust rust…".into(), target: Some(query.clone()) };
        assert_eq!(page_of(&search.tool, search.target(), None), Some(meerora_browser::url::search_url(&query)));
    }
}
