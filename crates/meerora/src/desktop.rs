//! The agent desktops of the nested compositor (D15 phase 2) as the app
//! sees them. [`Desktops`] owns the compositor, the textures of the
//! windows, the programs started from the launcher and the keyboard focus
//! rule. A window has the keyboard after a click into it, and loses it to
//! a click anywhere else, as the focused terminal does.
//!
//! The compositor starts with the first agent view that opens. Each agent
//! gets its desktop then, and a removed agent's desktop goes away with its
//! clients.
//!
//! The compositor runs on the main thread. Each frame dispatches it, and
//! [`Desktops::tick`] dispatches it between frames too, so apps keep
//! running while the meerora window is hidden and draws no frames. A small
//! thread wakes the event loop for that when nothing else does.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use meerora_compositor::{Compositor, Cursor, Event, Options, SandboxOptions, WindowInfo};

use crate::desktop_apps::{AppList, DesktopApp};

/// While a tile border is dragged, its window gets a new size at most this often.
const RESIZE_EVERY: Duration = Duration::from_millis(120);
/// A program that ends this soon without a window probably handed its
/// launch to an instance that runs outside meerora.
const QUICK_EXIT: Duration = Duration::from_secs(8);
/// Between frames the compositor is dispatched at most this often.
const TICK_EVERY: Duration = Duration::from_millis(10);
/// The wake thread wakes the event loop when the compositor waited this long.
const WAKE_AFTER: Duration = Duration::from_millis(40);
/// Partial texture uploads stop paying off above this share of the image.
const PARTIAL_UPLOAD_MAX: f32 = 0.5;

struct Texture {
    handle: egui::TextureHandle,
    serial: u64,
    /// Size of the image in points.
    size: egui::Vec2,
    /// Size in pixels.
    pixels: [usize; 2],
    nearest: bool,
}

/// What a window tile shows for the pointer, from [`Desktops::cursor`].
pub enum TileCursor {
    Icon(egui::CursorIcon),
    /// The client's own cursor image, drawn by the tile: its texture, its
    /// size in points and the hotspot in points.
    Image {
        texture: egui::TextureId,
        size: egui::Vec2,
        hotspot: egui::Vec2,
    },
}

/// Lets the wake thread see when the compositor ran last.
struct KeepAlive {
    /// Milliseconds since `start` of the last dispatch.
    last: Arc<AtomicU64>,
    /// The compositor has desktops, so dispatching matters.
    active: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    start: Instant,
}

struct Sizing {
    sent: (u32, u32),
    at: Instant,
}

struct Launched {
    agent: String,
    name: String,
    child: Child,
    started: Instant,
    /// A window appeared on the agent's desktop since the launch.
    window: bool,
}

pub struct Desktops {
    compositor: Option<Compositor>,
    /// The compositor could not start, which was said once.
    failed: bool,
    /// Agents whose desktop could not be made, so it is not tried every frame.
    failed_agents: HashSet<String>,
    options: Options,
    events: Vec<Event>,
    textures: HashMap<u32, Texture>,
    sizes: HashMap<u32, Sizing>,
    launched: Vec<Launched>,
    toasts: Vec<(String, bool)>,
    data_dir: PathBuf,
    /// The installed apps for the launcher, scanned when it first opens.
    pub apps: Option<AppList>,
    /// The search text of "Your apps".
    pub search: String,
    /// `[desktop] inhibit_shortcuts` of meerora.toml.
    pub inhibit_shortcuts: bool,
    /// The window a pointer button went down on, it gets the motion until the button is up.
    pressed_on: Option<u32>,
    /// The window under the pointer in the last frame.
    hovered: Option<u32>,
    hovered_now: Option<u32>,
    /// Windows whose tile was drawn in this frame.
    drawn: HashSet<u32>,
    /// A window tile took this frame's pointer press.
    claimed: bool,
    /// The window that had the keyboard when the meerora window lost it.
    resume: Option<u32>,
    last_dispatch: Instant,
    keep_alive: Option<KeepAlive>,
    /// The texture of a client's cursor image and its serial.
    cursor_texture: Option<(egui::TextureHandle, u64)>,
}

impl Desktops {
    pub fn new(data_dir: &Path) -> Self {
        let settings = meerora_protocol::Settings::load(data_dir).unwrap_or_default().desktop;
        let options = Options {
            keyboard_layout: settings.keyboard_layout.unwrap_or_else(|| "us".into()),
            keyboard_variant: settings.keyboard_variant.unwrap_or_default(),
        };
        Self {
            compositor: None,
            failed: false,
            failed_agents: HashSet::new(),
            options,
            events: Vec::new(),
            textures: HashMap::new(),
            sizes: HashMap::new(),
            launched: Vec::new(),
            toasts: Vec::new(),
            data_dir: data_dir.to_path_buf(),
            apps: None,
            search: String::new(),
            inhibit_shortcuts: settings.inhibit_shortcuts,
            pressed_on: None,
            hovered: None,
            hovered_now: None,
            drawn: HashSet::new(),
            claimed: false,
            resume: None,
            last_dispatch: Instant::now(),
            keep_alive: None,
            cursor_texture: None,
        }
    }

    /// Starts the thread that wakes the event loop through `wake` when the
    /// compositor was not dispatched for a while, for example because the
    /// meerora window is hidden and draws no frames. The loop then calls
    /// [`Self::tick`].
    pub fn keep_alive(&mut self, wake: impl Fn() + Send + 'static) {
        if self.keep_alive.is_some() {
            return;
        }
        let start = Instant::now();
        let (last, active, stop) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
        let (l, a, s) = (last.clone(), active.clone(), stop.clone());
        let spawned = std::thread::Builder::new().name("meerora-desktops".into()).spawn(move || {
            while !s.load(Ordering::Relaxed) {
                std::thread::sleep(WAKE_AFTER / 2);
                let idle = (start.elapsed().as_millis() as u64).saturating_sub(l.load(Ordering::Relaxed));
                if a.load(Ordering::Relaxed) && idle >= WAKE_AFTER.as_millis() as u64 {
                    wake();
                }
            }
        });
        match spawned {
            Ok(_) => self.keep_alive = Some(KeepAlive { last, active, stop, start }),
            Err(e) => tracing::warn!("no wake thread for the desktops: {e}"),
        }
    }

    /// Dispatches the compositor between frames, unless that happened just
    /// now. Call it as often as the event loop wakes.
    pub fn tick(&mut self) {
        if self.compositor.is_some() && self.last_dispatch.elapsed() >= TICK_EVERY {
            self.dispatch();
        }
    }

    /// Creates the desktop of `agent`, and the compositor with the first one.
    /// Returns false when there is none.
    pub fn ensure(&mut self, agent: &str) -> bool {
        if self.compositor.is_none() && !self.failed {
            match Compositor::with_options(self.options.clone()) {
                Ok(c) => self.compositor = Some(c),
                Err(e) => {
                    self.failed = true;
                    self.toasts.push((format!("Your apps cannot run in agent views: {e}"), true));
                }
            }
        }
        let Some(compositor) = self.compositor.as_mut() else { return false };
        if compositor.socket(agent).is_some() {
            return true;
        }
        if self.failed_agents.contains(agent) {
            return false;
        }
        match compositor.add_desktop(agent) {
            Ok(_) => true,
            Err(e) => {
                self.toasts.push((format!("No desktop for {agent}: {e}"), true));
                self.failed_agents.insert(agent.to_string());
                false
            }
        }
    }

    /// Handles the clients' requests, once per frame, and notices programs
    /// that ended without showing a window.
    pub fn dispatch(&mut self) {
        self.last_dispatch = Instant::now();
        let Some(compositor) = self.compositor.as_mut() else { return };
        compositor.dispatch();
        if let Some(k) = &self.keep_alive {
            k.last.store(k.start.elapsed().as_millis() as u64, Ordering::Relaxed);
            k.active.store(!compositor.desktops().is_empty(), Ordering::Relaxed);
        }
        let events = compositor.take_events();
        for e in &events {
            if let Event::Opened(id) = e
                && let Some(w) = compositor.window(*id)
            {
                for l in self.launched.iter_mut().filter(|l| l.agent == w.desktop) {
                    l.window = true;
                }
            }
        }
        self.events.extend(events);
        let toasts = &mut self.toasts;
        self.launched.retain_mut(|l| match l.child.try_wait() {
            Ok(None) => true,
            Ok(Some(status)) => {
                if !l.window && l.started.elapsed() < QUICK_EXIT {
                    let how = status.code().map_or_else(|| "was stopped".to_string(), |c| format!("exited with {c}"));
                    toasts.push((
                        format!(
                            "{} {how} without a window. An app that already runs outside meerora may have opened its window there.",
                            l.name
                        ),
                        status.code() != Some(0),
                    ));
                }
                false
            }
            Err(_) => false,
        });
    }

    pub fn take_events(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.events)
    }

    pub fn take_toasts(&mut self) -> Vec<(String, bool)> {
        std::mem::take(&mut self.toasts)
    }

    /// Removes `agent`'s desktop. Its windows close and the programs
    /// started on it end.
    pub fn remove(&mut self, agent: &str) {
        self.failed_agents.remove(agent);
        let Some(compositor) = self.compositor.as_mut() else { return };
        compositor.remove_desktop(agent);
        self.events.extend(compositor.take_events());
        for l in self.launched.iter_mut().filter(|l| l.agent == agent) {
            meerora_compositor::launch::stop(&mut l.child, Duration::from_millis(500));
        }
        self.launched.retain(|l| l.agent != agent);
    }

    pub fn window(&self, id: u32) -> Option<WindowInfo> {
        self.compositor.as_ref()?.window(id)
    }

    pub fn windows(&self) -> Vec<WindowInfo> {
        self.compositor.as_ref().map(Compositor::windows).unwrap_or_default()
    }

    /// The name on the window's tab and tile head.
    pub fn title(&self, id: u32) -> String {
        match self.window(id) {
            Some(w) if !w.title.is_empty() => w.title,
            Some(w) if !w.app_id.is_empty() => w.app_id,
            _ => "Window".into(),
        }
    }

    /// Drops what the app keeps for a closed window.
    pub fn forget(&mut self, id: u32) {
        self.textures.remove(&id);
        self.sizes.remove(&id);
        if self.pressed_on == Some(id) {
            self.pressed_on = None;
        }
    }

    /// The window's newest image as a texture, uploaded only when it changed
    /// and then only where it changed, with its size in points.
    pub fn texture(&mut self, ctx: &egui::Context, id: u32) -> Option<(egui::TextureId, egui::Vec2)> {
        let frame = self.compositor.as_mut()?.frame(id)?;
        let ppp = ctx.pixels_per_point();
        // Pixel for pixel when the window renders at the screen's scale.
        let nearest = (frame.scale - ppp).abs() < 0.01;
        let size = egui::vec2(frame.width as f32 / frame.scale, frame.height as f32 / frame.scale);
        let pixels = [frame.width as usize, frame.height as usize];
        let options = if nearest { egui::TextureOptions::NEAREST } else { egui::TextureOptions::LINEAR };
        match self.textures.get_mut(&id) {
            Some(t) if t.serial == frame.serial && t.nearest == nearest => {}
            Some(t) => {
                let damage = (t.pixels == pixels && t.nearest == nearest).then(|| frame.damage_since(t.serial)).flatten();
                let area: u64 = damage.iter().flatten().map(|r| u64::from(r[2]) * u64::from(r[3])).sum();
                match damage {
                    Some(rects) if (area as f32) < PARTIAL_UPLOAD_MAX * (pixels[0] * pixels[1]) as f32 => {
                        for [x, y, w, h] in rects {
                            let image = sub_image(frame.pixels, frame.width, [x, y, w, h]);
                            t.handle.set_partial([x as usize, y as usize], image, options);
                        }
                    }
                    _ => t.handle.set(egui::ColorImage::from_rgba_premultiplied(pixels, frame.pixels), options),
                }
                (t.serial, t.size, t.pixels, t.nearest) = (frame.serial, size, pixels, nearest);
            }
            None => {
                let image = egui::ColorImage::from_rgba_premultiplied(pixels, frame.pixels);
                let handle = ctx.load_texture(format!("window-{id}"), image, options);
                self.textures.insert(id, Texture { handle, serial: frame.serial, size, pixels, nearest });
            }
        }
        self.textures.get(&id).map(|t| (t.handle.id(), t.size))
    }

    /// Asks the window to fill its tile, `size` in points. While a border is
    /// dragged the window follows a few times a second, and exactly once the
    /// button is up.
    pub fn fit(&mut self, id: u32, size: (u32, u32), dragging: bool) {
        let Some(compositor) = self.compositor.as_mut() else { return };
        let size = (size.0.max(1), size.1.max(1));
        let due = match self.sizes.get(&id) {
            None => true,
            Some(s) if s.sent == size => false,
            Some(s) => !dragging || s.at.elapsed() >= RESIZE_EVERY,
        };
        if due {
            compositor.configure(id, size.0, size.1);
            self.sizes.insert(id, Sizing { sent: size, at: Instant::now() });
        }
    }

    /// The window took the size its tile asked for, within a character
    /// cell, since terminals round to whole cells.
    pub fn fits(&mut self, id: u32) -> bool {
        let Some(sent) = self.sizes.get(&id).map(|s| s.sent) else { return false };
        let Some(frame) = self.compositor.as_mut().and_then(|c| c.frame(id)) else { return false };
        let (w, h) = ((frame.width as f32 / frame.scale).round() as u32, (frame.height as f32 / frame.scale).round() as u32);
        w.abs_diff(sent.0) <= 24 && h.abs_diff(sent.1) <= 32
    }

    /// The window's tile was drawn, so its frame callbacks fire.
    pub fn drawn(&mut self, id: u32) {
        self.drawn.insert(id);
        if let Some(c) = self.compositor.as_mut() {
            c.drawn(id);
        }
    }

    /// Sets the virtual output to the tile area and the screen's scale.
    pub fn set_output(&mut self, width: f32, height: f32, pixels_per_point: f32) {
        if let Some(c) = self.compositor.as_mut() {
            c.set_output(width.max(1.0) as u32, height.max(1.0) as u32, pixels_per_point);
        }
    }

    pub fn focused(&self) -> Option<u32> {
        self.compositor.as_ref().and_then(Compositor::focused)
    }

    pub fn focus(&mut self, id: Option<u32>) {
        if let Some(c) = self.compositor.as_mut() {
            c.focus(id);
        }
    }

    /// The meerora window got or lost the host's keyboard. The focused
    /// window lets go of its keys meanwhile and gets the keyboard back.
    pub fn host_focus(&mut self, focused: bool) {
        if focused {
            if let Some(id) = self.resume.take() {
                self.focus(Some(id));
            }
        } else if let Some(id) = self.focused() {
            self.resume = Some(id);
            self.focus(None);
        }
    }

    /// Sends a key (evdev code) to the focused window. Returns false when no window has the keyboard.
    pub fn key(&mut self, evdev: u32, pressed: bool) -> bool {
        let Some(id) = self.focused() else { return false };
        if let Some(c) = self.compositor.as_mut() {
            c.key(id, evdev, pressed);
        }
        true
    }

    /// Puts the host clipboard's text on the desktops' clipboard, before a paste.
    pub fn set_selection(&mut self, text: &str) {
        if let Some(c) = self.compositor.as_mut() {
            c.set_selection(text);
        }
    }

    pub fn compositor(&mut self) -> Option<&mut Compositor> {
        self.compositor.as_mut()
    }

    /// The XKB layout and variant of the desktops, which decide what an evdev code types.
    pub fn keyboard_layout(&self) -> (&str, &str) {
        (&self.options.keyboard_layout, &self.options.keyboard_variant)
    }

    /// What the window under the pointer wants to show as the pointer: a
    /// named shape as an egui cursor, or its own image as a texture.
    pub fn cursor(&mut self, ctx: &egui::Context) -> TileCursor {
        let Some(compositor) = self.compositor.as_mut() else { return TileCursor::Icon(egui::CursorIcon::Default) };
        let image = match compositor.cursor() {
            Cursor::Default => return TileCursor::Icon(egui::CursorIcon::Default),
            Cursor::Hidden => return TileCursor::Icon(egui::CursorIcon::None),
            Cursor::Named(name) => return TileCursor::Icon(cursor_icon(name)),
            Cursor::Image(image) => image,
        };
        let pixels = [image.width as usize, image.height as usize];
        let stale = self.cursor_texture.as_ref().is_none_or(|(_, serial)| *serial != image.serial);
        if stale {
            let color = egui::ColorImage::from_rgba_premultiplied(pixels, image.pixels);
            match self.cursor_texture.as_mut() {
                Some((handle, serial)) => {
                    handle.set(color, egui::TextureOptions::LINEAR);
                    *serial = image.serial;
                }
                None => self.cursor_texture = Some((ctx.load_texture("window-cursor", color, egui::TextureOptions::LINEAR), image.serial)),
            }
        }
        let texture = self.cursor_texture.as_ref().map(|(h, _)| h.id()).expect("made above");
        TileCursor::Image {
            texture,
            size: egui::vec2(image.width as f32 / image.scale, image.height as f32 / image.scale),
            hotspot: egui::vec2(image.hotspot.0, image.hotspot.1),
        }
    }

    /// Starts an installed app on `agent`'s desktop in `cwd`.
    pub fn launch(&mut self, agent: &str, app: &DesktopApp, cwd: Option<&Path>) -> Result<(), String> {
        let (program, args) = app.exec.split_first().ok_or_else(|| format!("{} has no command", app.name))?;
        self.launch_command(agent, &app.name, program, args, cwd)?;
        if let Some(list) = self.apps.as_mut() {
            list.used(&app.id);
        }
        Ok(())
    }

    /// Starts `program` with `args` on `agent`'s desktop.
    pub fn launch_command(&mut self, agent: &str, name: &str, program: &str, args: &[String], cwd: Option<&Path>) -> Result<(), String> {
        if !self.ensure(agent) {
            return Err(format!("{name} cannot start: there is no desktop for {agent}"));
        }
        let compositor = self.compositor.as_mut().expect("ensure made it");
        let child = compositor.launch(agent, program, args, cwd).map_err(|e| format!("{name} cannot start: {e}"))?;
        tracing::info!("started {name} ({program}) on the desktop of {agent}, pid {}", child.id());
        self.launched.push(Launched { agent: agent.to_string(), name: name.to_string(), child, started: Instant::now(), window: false });
        Ok(())
    }

    /// Starts `program` for `agent` in its sandbox on its desktop, with
    /// `agent_dir` as its home (D8, D16). The caller checks that the agent
    /// holds `apps.launch`. The agent tool `app_open` calls it (agent_apps.rs, D30).
    pub fn launch_sandboxed(
        &mut self,
        agent: &str,
        agent_dir: &Path,
        name: &str,
        program: &str,
        args: &[String],
        options: &SandboxOptions,
    ) -> Result<(), String> {
        if !self.ensure(agent) {
            return Err(format!("{name} cannot start: there is no desktop for {agent}"));
        }
        let compositor = self.compositor.as_mut().expect("ensure made it");
        let child =
            compositor.launch_sandboxed(agent, agent_dir, program, args, options).map_err(|e| format!("{name} cannot start: {e}"))?;
        tracing::info!("started {name} ({program}) for {agent} in its sandbox, pid {}", child.id());
        self.launched.push(Launched { agent: agent.to_string(), name: name.to_string(), child, started: Instant::now(), window: false });
        Ok(())
    }

    /// The app list for the launcher, scanned on first use.
    pub fn app_list(&mut self) -> &mut AppList {
        let data_dir = self.data_dir.clone();
        let list = self.apps.get_or_insert_with(|| AppList::load(&data_dir));
        list.refresh();
        list
    }

    // Pointer routing. The window tiles call these with egui's events, in
    // window coordinates (points from the image's top left).

    pub fn pointer_motion(&mut self, id: u32, x: f32, y: f32) {
        self.hovered_now = Some(id);
        if let Some(c) = self.compositor.as_mut() {
            c.pointer_motion(id, f64::from(x), f64::from(y));
        }
    }

    pub fn pointer_button(&mut self, id: u32, button: u32, pressed: bool) {
        if pressed {
            self.pressed_on = Some(id);
        } else if self.pressed_on == Some(id) {
            self.pressed_on = None;
        }
        if let Some(c) = self.compositor.as_mut() {
            c.pointer_button(id, button, pressed);
        }
    }

    /// The window that holds the pointer while a button is down on it.
    pub fn pressed_on(&self) -> Option<u32> {
        self.pressed_on
    }

    pub fn scroll(&mut self, id: u32, dx: f64, dy: f64) {
        if let Some(c) = self.compositor.as_mut() {
            c.scroll(id, dx, dy, None);
        }
    }

    /// Scrolls by mouse wheel clicks, fractions for high resolution wheels.
    pub fn scroll_clicks(&mut self, id: u32, x: f64, y: f64) {
        if let Some(c) = self.compositor.as_mut() {
            c.scroll_clicks(id, x, y);
        }
    }

    /// A window tile took the pointer press of this frame, so its window keeps the keyboard.
    pub fn claim_press(&mut self) {
        self.claimed = true;
    }

    pub fn begin_frame(&mut self) {
        self.drawn.clear();
        self.claimed = false;
        self.hovered_now = None;
    }

    /// The focus rule, after all tiles drew. A press outside every window
    /// tile takes the keyboard from the window, and so does hiding its tile.
    /// The pointer leaves windows it is no longer over.
    pub fn end_frame(&mut self, pressed: bool) {
        if let Some(id) = self.focused()
            && ((pressed && !self.claimed) || !self.drawn.contains(&id))
        {
            self.focus(None);
        }
        if self.hovered_now.is_none()
            && self.pressed_on.is_none()
            && self.hovered.is_some()
            && let Some(c) = self.compositor.as_mut()
        {
            c.pointer_leave();
        }
        if self.pressed_on.is_some_and(|id| !self.drawn.contains(&id)) {
            self.pressed_on = None;
        }
        self.hovered = self.hovered_now;
    }
}

impl Drop for Desktops {
    /// Ends the programs started from the launcher. Their clients would
    /// also end when the sockets close.
    fn drop(&mut self) {
        if let Some(k) = &self.keep_alive {
            k.stop.store(true, Ordering::Relaxed);
        }
        for l in &mut self.launched {
            meerora_compositor::launch::stop(&mut l.child, Duration::from_millis(300));
        }
    }
}

/// One region of a window image as its own image, for a partial upload.
fn sub_image(pixels: &[u8], width: u32, [x, y, w, h]: [u32; 4]) -> egui::ColorImage {
    let mut out = Vec::with_capacity(w as usize * h as usize * 4);
    for row in y..y + h {
        let start = (row as usize * width as usize + x as usize) * 4;
        out.extend_from_slice(&pixels[start..start + w as usize * 4]);
    }
    egui::ColorImage::from_rgba_premultiplied([w as usize, h as usize], &out)
}

/// egui's cursor for a CSS cursor name from `wp_cursor_shape_v1`.
fn cursor_icon(name: &str) -> egui::CursorIcon {
    use egui::CursorIcon as C;
    match name {
        "context-menu" => C::ContextMenu,
        "help" => C::Help,
        "pointer" => C::PointingHand,
        "progress" => C::Progress,
        "wait" => C::Wait,
        "cell" => C::Cell,
        "crosshair" => C::Crosshair,
        "text" => C::Text,
        "vertical-text" => C::VerticalText,
        "alias" => C::Alias,
        "copy" => C::Copy,
        "move" | "all-resize" => C::Move,
        "no-drop" => C::NoDrop,
        "not-allowed" => C::NotAllowed,
        "grab" => C::Grab,
        "grabbing" => C::Grabbing,
        "all-scroll" => C::AllScroll,
        "ew-resize" => C::ResizeHorizontal,
        "ns-resize" => C::ResizeVertical,
        "nesw-resize" => C::ResizeNeSw,
        "nwse-resize" => C::ResizeNwSe,
        "e-resize" => C::ResizeEast,
        "se-resize" => C::ResizeSouthEast,
        "s-resize" => C::ResizeSouth,
        "sw-resize" => C::ResizeSouthWest,
        "w-resize" => C::ResizeWest,
        "nw-resize" => C::ResizeNorthWest,
        "n-resize" => C::ResizeNorth,
        "ne-resize" => C::ResizeNorthEast,
        "col-resize" => C::ResizeColumn,
        "row-resize" => C::ResizeRow,
        "zoom-in" => C::ZoomIn,
        "zoom-out" => C::ZoomOut,
        _ => C::Default,
    }
}

/// For `meerora snapshot --launch`. Starts `command` on the desktop of the
/// agent whose view shows and runs frames until its window shows in a tile
/// at the tile's size and had a moment to draw its content. A `;` between
/// words starts several programs, and it waits for all their windows.
pub fn snapshot_launch(
    core: &mut crate::engine::Core,
    ctx: &egui::Context,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    screen: egui::Rect,
    scale: f32,
    command: &[String],
) -> anyhow::Result<()> {
    use anyhow::Context as _;
    let crate::ui::View::Agent(agent) = core.ui.view.clone() else { anyhow::bail!("--launch needs --view agent:ID") };
    let cwd = core.agent_dir(&agent).map(|d| d.path.clone());
    let programs: Vec<&[String]> = command.split(|word| word == ";").filter(|c| !c.is_empty()).collect();
    for command in &programs {
        let (program, args) = command.split_first().context("--launch needs a program")?;
        core.desktops.launch_command(&agent, program, program, args, cwd.as_deref()).map_err(anyhow::Error::msg)?;
    }
    let program = programs.iter().filter_map(|c| c.first()).cloned().collect::<Vec<_>>().join(", ");
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut ready_since: Option<Instant> = None;
    loop {
        core.step(device, queue, 1.0 / 30.0);
        let mut raw = egui::RawInput { screen_rect: Some(screen), ..Default::default() };
        raw.viewports.entry(egui::ViewportId::ROOT).or_default().native_pixels_per_point = Some(scale);
        let out = ctx.run_ui(raw, |ui| core.build_ui(ui));
        core.apply_textures(device, queue, out.textures_delta);
        let windows: Vec<u32> = core.desktops.windows().iter().filter(|w| w.desktop == agent).map(|w| w.id).collect();
        let ready = windows.len() >= programs.len() && windows.iter().all(|id| core.desktops.fits(*id));
        match (ready, ready_since) {
            (true, None) => ready_since = Some(Instant::now()),
            (true, Some(since)) if since.elapsed() > Duration::from_millis(1500) => return Ok(()),
            (false, _) => ready_since = None,
            _ => {}
        }
        if Instant::now() > deadline {
            anyhow::bail!("{program} showed no window on the desktop of {agent} within 30 s");
        }
        std::thread::sleep(Duration::from_millis(30));
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;

    use egui::{Event, Key, Modifiers, PointerButton, Pos2, Rect, pos2, vec2};

    use crate::assets::Assets;
    use crate::engine::Core;
    use crate::ui::{AppTab, View};

    #[test]
    fn cursor_names_become_egui_cursors() {
        assert_eq!(super::cursor_icon("text"), egui::CursorIcon::Text);
        assert_eq!(super::cursor_icon("pointer"), egui::CursorIcon::PointingHand);
        assert_eq!(super::cursor_icon("nwse-resize"), egui::CursorIcon::ResizeNwSe);
        assert_eq!(super::cursor_icon("something-new"), egui::CursorIcon::Default);
    }

    #[test]
    fn a_damaged_region_is_cut_out_for_upload() {
        // A 3x2 image whose pixels count up, cut at x 1, y 0, 2 wide and 2 high.
        let pixels: Vec<u8> = (0..6u8).flat_map(|i| [i, i, i, 255]).collect();
        let part = super::sub_image(&pixels, 3, [1, 0, 2, 2]);
        assert_eq!(part.size, [2, 2]);
        let reds: Vec<u8> = part.pixels.iter().map(|p| p.r()).collect();
        assert_eq!(reds, [1, 2, 4, 5]);
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

    /// One UI frame with these input events, as the window would run it.
    fn frame(core: &mut Core, ctx: &egui::Context, gpu: &meerora_scene::Gpu, events: Vec<Event>) {
        core.step(&gpu.device, &gpu.queue, 1.0 / 30.0);
        let screen = Rect::from_min_size(Pos2::ZERO, vec2(1440.0, 900.0));
        let raw = egui::RawInput { screen_rect: Some(screen), events, ..Default::default() };
        let out = ctx.run_ui(raw, |ui| core.build_ui(ui));
        core.apply_textures(&gpu.device, &gpu.queue, out.textures_delta);
    }

    fn click(core: &mut Core, ctx: &egui::Context, gpu: &meerora_scene::Gpu, at: Pos2) {
        let button = |pressed| Event::PointerButton { pos: at, button: PointerButton::Primary, pressed, modifiers: Modifiers::NONE };
        frame(core, ctx, gpu, vec![Event::PointerMoved(at)]);
        frame(core, ctx, gpu, vec![button(true)]);
        frame(core, ctx, gpu, vec![button(false)]);
    }

    /// A real client in a real agent view. It opens as the selected tile at
    /// the tile's size, a click gives it the keyboard, a click on the chat
    /// takes it back, Alt+Tab switches tabs, and the cross closes it.
    #[test]
    fn a_window_tile_takes_and_gives_back_the_keyboard_and_closes() {
        let found = std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join("weston-terminal").is_file()));
        if !found {
            eprintln!("skipped: weston-terminal is not installed");
            return;
        }
        let Ok(gpu) = meerora_scene::Gpu::headless() else {
            eprintln!("skipped: no headless GPU");
            return;
        };
        let assets = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets");
        let data = std::env::temp_dir().join(format!("meerora-desktop-test-{}", std::process::id()));
        copy_dir(&assets.join("spaces/workshop"), &data.join("spaces/workshop"));
        copy_dir(&assets.join("agents/wren"), &data.join("agents/wren"));
        let toml = data.join("agents/wren/agent.toml");
        let text = std::fs::read_to_string(&toml).unwrap().replace("home = \"window\"", "home = \"window\"\napps = [\"chat\"]");
        std::fs::write(&toml, text).unwrap();

        let mut core = Core::new(&gpu.device, meerora_scene::IMAGE_FORMAT, Assets::without_watcher(data.clone()), None, Arc::new(|| {}));
        core.load(&gpu.device, &gpu.queue);
        core.ui.view = View::Agent("wren".into());
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        let screen = Rect::from_min_size(Pos2::ZERO, vec2(1440.0, 900.0));
        // Its own home, so the shell writes no history into the user's.
        let home = data.join("home");
        std::fs::create_dir_all(&home).unwrap();
        let command =
            ["env", &format!("HOME={}", home.display()), "HISTFILE=/dev/null", "weston-terminal", "--shell=/bin/sh"].map(String::from);
        super::snapshot_launch(&mut core, &ctx, &gpu.device, &gpu.queue, screen, 1.0, &command).unwrap();

        let id = core.desktops.windows().iter().find(|w| w.desktop == "wren").map(|w| w.id).expect("a window on wren's desktop");
        assert_eq!(core.ui.tab, AppTab::Window(id), "the new window is the selected tab");
        let content = crate::ui::agent::content_rect(screen);
        let rects = crate::ui::tiles::layout_for(&mut core, "wren", &[AppTab::Chat], content).rects(content);
        let tile = rects.iter().find(|(t, _)| *t == AppTab::Window(id)).map(|(_, r)| *r).expect("a tile for the window");
        let chat = rects.iter().find(|(t, _)| *t == AppTab::Chat).map(|(_, r)| *r).unwrap();
        assert!(core.desktops.fits(id), "the window took its tile's size");

        assert_eq!(core.desktops.focused(), None);
        click(&mut core, &ctx, &gpu, tile.center());
        assert_eq!(core.desktops.focused(), Some(id), "a click into the tile gives the window the keyboard");
        click(&mut core, &ctx, &gpu, pos2(chat.center().x, chat.min.y + 200.0));
        assert_eq!(core.desktops.focused(), None, "a click on the chat takes it back");

        click(&mut core, &ctx, &gpu, tile.center());
        assert_eq!(core.desktops.focused(), Some(id));
        // Alt+` cycles the tabs of the agent view (Alt+Tab switches agents).
        let alt_grave = Event::Key { key: Key::Backtick, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::ALT };
        frame(&mut core, &ctx, &gpu, vec![alt_grave]);
        assert_eq!(core.ui.tab, AppTab::Chat, "Alt+` goes to the next tab, around to the chat");
        assert_eq!(core.desktops.focused(), None, "and the window lets go of the keyboard");

        // The cross at the right end of the tile head asks the window to close.
        let cross = pos2(tile.max.x - 20.0, tile.min.y + 19.0);
        click(&mut core, &ctx, &gpu, cross);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while core.desktops.window(id).is_some() && std::time::Instant::now() < deadline {
            frame(&mut core, &ctx, &gpu, Vec::new());
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        frame(&mut core, &ctx, &gpu, Vec::new());
        assert!(core.desktops.window(id).is_none(), "the close request ended weston-terminal");
        let apps = crate::ui::tiles::layout_for(&mut core, "wren", &[AppTab::Chat], content).apps();
        assert_eq!(apps, [AppTab::Chat], "the tile went with the window");
        drop(core);
        std::fs::remove_dir_all(&data).unwrap();
    }
}
