//! The nested Wayland compositor of D15 phase 2, built on Smithay (D16).
//!
//! Every agent gets a desktop. A desktop has two realms that share nothing
//! but the compositor:
//! - the user realm for apps the user starts as normal programs, with the
//!   Wayland socket `meerora-wl-<agent>` in `$XDG_RUNTIME_DIR`, an X display
//!   and a private session bus
//! - the agent realm for apps an agent starts with `apps.launch` (D8) in
//!   bubblewrap, with a Wayland socket, an X display and a session bus of
//!   its own in a runtime directory that the sandbox sees
//!
//! Apps connect and become windows, which the meerora app draws as tiles of
//! the agent's view (D11). X11 apps go through Xwayland and the X11 window
//! manager in `xwayland.rs`. The compositor has no window and no GPU
//! renderer of its own. It composes each window from the shared memory
//! buffers of its surfaces into one RGBA image, which the app uploads as a
//! texture, and it tells the app which regions changed.
//!
//! It runs on the caller's thread. [`Compositor::dispatch`] never blocks.
//! The app calls it once per frame and also while it draws nothing, for
//! example while its window is hidden, so apps keep running. Input arrives
//! in window coordinates, already routed by the app. The app knows which
//! tile is under the pointer and which one has the keyboard.

mod bus;
mod compose;
mod focus;
pub mod launch;
mod state;
mod xwayland;

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::Arc;
use std::time::{Duration, Instant};

use smithay::backend::input::{Axis, AxisSource, ButtonState, KeyState};
use smithay::desktop::utils::under_from_surface_tree;
use smithay::desktop::{PopupManager, WindowSurfaceType};
use smithay::input::keyboard::{FilterResult, Keycode, XkbConfig};
use smithay::input::pointer::{AxisFrame, ButtonEvent, CursorIcon, CursorImageStatus, CursorImageSurfaceData, MotionEvent};
use smithay::input::{Seat, SeatState};
use smithay::output::{Mode, Output, PhysicalProperties, Scale, Subpixel};
use smithay::reexports::calloop::EventLoop;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::backend::DisconnectReason;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{Display, ListeningSocket};
use smithay::utils::{IsAlive as _, Logical, Point, Rectangle, SERIAL_COUNTER, Size, Transform};
use smithay::wayland::compositor::{CompositorClientState, CompositorState, with_states};
use smithay::wayland::cursor_shape::CursorShapeManagerState;
use smithay::wayland::fractional_scale::FractionalScaleManagerState;
use smithay::wayland::output::OutputManagerState;
use smithay::wayland::seat::WaylandFocus as _;
use smithay::wayland::selection::SelectionTarget;
use smithay::wayland::selection::data_device::{DataDeviceState, set_data_device_selection};
use smithay::wayland::selection::primary_selection::{PrimarySelectionState, set_primary_selection};
use smithay::wayland::shell::xdg::XdgShellState;
use smithay::wayland::shell::xdg::decoration::XdgDecorationState;
use smithay::wayland::shm::ShmState;
use smithay::wayland::viewporter::ViewporterState;
use smithay::wayland::xwayland_shell::XWaylandShellState;
use smithay::xwayland::X11Wm;

pub use compose::Rect;
pub use launch::{LaunchOptions, SandboxOptions, X11};

use focus::Focus;
use state::{ClientState, Clipboard, CursorState, Desktop, HISTORY, Offer, Owner, Place, State, TEXT_MIMES};
use xwayland::{XOwner, XServer};

/// What happened since the last [`Compositor::take_events`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// A window got its first buffer and should show.
    Opened(u32),
    /// A window was unmapped or destroyed. A Wayland window that maps again
    /// comes back with the same id, an X11 window with a new one.
    Closed(u32),
    /// Title or app id changed.
    Changed(u32),
    /// A client put text on the clipboard.
    Selection(String),
    /// A client selected text, the primary selection for middle-click paste.
    PrimarySelection(String),
}

/// Who may use a part of a desktop. See the crate documentation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Realm {
    /// Apps the user starts, as normal programs.
    User,
    /// Apps an agent starts, in its sandbox.
    Agent,
}

/// A shown window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WindowInfo {
    pub id: u32,
    /// The agent whose desktop the window's client connected to.
    pub desktop: String,
    pub title: String,
    /// The app id, or the class of an X11 window.
    pub app_id: String,
    /// Window geometry in logical units (points in the app).
    pub size: (u32, u32),
    /// It came through Xwayland.
    pub x11: bool,
    /// An agent started it, in its sandbox ([`Realm::Agent`]).
    pub sandboxed: bool,
}

/// The latest image of a window in premultiplied RGBA, `width * height * 4`
/// bytes at `scale` pixels per logical unit. `serial` changes with every
/// new composition, so the app uploads only new images, and
/// [`Frame::damage_since`] says which parts changed.
pub struct Frame<'a> {
    pub serial: u64,
    pub width: u32,
    pub height: u32,
    /// The density of the window's own buffer, for example 1.25 for a
    /// client that renders at the output's fractional scale and 1 for X11.
    pub scale: f32,
    pub pixels: &'a [u8],
    history: &'a VecDeque<(u64, Option<Vec<Rect>>)>,
}

impl Frame<'_> {
    /// The regions that changed since the image with `serial`, in pixels,
    /// or `None` when the app should upload the whole image.
    pub fn damage_since(&self, serial: u64) -> Option<Vec<Rect>> {
        if serial == self.serial {
            return Some(Vec::new());
        }
        let mut next = serial.checked_add(1)?;
        let mut out = Vec::new();
        for (s, damage) in self.history.iter().filter(|(s, _)| *s > serial && *s <= self.serial) {
            if *s != next {
                return None;
            }
            out.extend(damage.as_ref()?.iter().copied());
            next += 1;
        }
        (next == self.serial + 1).then_some(out)
    }
}

/// The pointer image that the client under the pointer asked for.
pub enum Cursor<'a> {
    /// No special cursor, the app shows its arrow.
    Default,
    Hidden,
    /// A shape from `wp_cursor_shape_v1`, by its CSS name, such as `text`
    /// or `pointer`.
    Named(&'static str),
    /// An image the client drew, from `wl_pointer.set_cursor`.
    Image(CursorImage<'a>),
}

/// A cursor image in premultiplied RGBA at `scale` pixels per logical unit.
pub struct CursorImage<'a> {
    /// Changes with every new image.
    pub serial: u64,
    pub width: u32,
    pub height: u32,
    pub scale: f32,
    /// The point of the image at the pointer, in logical units.
    pub hotspot: (f32, f32),
    pub pixels: &'a [u8],
}

/// Settings for [`Compositor::with_options`].
#[derive(Clone, Debug)]
pub struct Options {
    /// XKB layout of the keymap clients get, for example `us` or `de`.
    pub keyboard_layout: String,
    /// XKB variant, for example `nodeadkeys`. Empty for the default.
    pub keyboard_variant: String,
}

impl Default for Options {
    fn default() -> Self {
        Self { keyboard_layout: "us".into(), keyboard_variant: String::new() }
    }
}

#[derive(Debug)]
pub enum Error {
    /// `$XDG_RUNTIME_DIR` is not set, so there is nowhere for sockets.
    NoRuntimeDir,
    Display(String),
    Socket(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::NoRuntimeDir => write!(f, "XDG_RUNTIME_DIR is not set, so the compositor has no place for its sockets"),
            Error::Display(e) => write!(f, "cannot create the Wayland display: {e}"),
            Error::Socket(e) => write!(f, "cannot create the desktop socket: {e}"),
        }
    }
}

impl std::error::Error for Error {}

/// Linux button codes for [`Compositor::pointer_button`].
pub const BTN_LEFT: u32 = 0x110;
pub const BTN_RIGHT: u32 = 0x111;
pub const BTN_MIDDLE: u32 = 0x112;
pub const BTN_SIDE: u32 = 0x113;
pub const BTN_EXTRA: u32 = 0x114;

/// Logical pixels one wheel step scrolls, as most compositors send.
const WHEEL_STEP: f64 = 15.0;
/// Windows whose tile is hidden still get frame callbacks this often, so
/// apps that wait for one before they go on keep running.
const HIDDEN_FRAME_EVERY: Duration = Duration::from_secs(1);

pub struct Compositor {
    display: Display<State>,
    /// For the X11 window managers and their selection transfers.
    event_loop: EventLoop<'static, State>,
    state: State,
    _viewporter: ViewporterState,
    _outputs: OutputManagerState,
    _fractional: FractionalScaleManagerState,
    _cursor_shape: CursorShapeManagerState,
}

impl Compositor {
    /// A compositor with the `us` keymap and no desktops yet.
    pub fn new() -> Result<Self, Error> {
        Self::with_options(Options::default())
    }

    pub fn with_options(options: Options) -> Result<Self, Error> {
        if std::env::var_os("XDG_RUNTIME_DIR").is_none() {
            return Err(Error::NoRuntimeDir);
        }
        let display: Display<State> = Display::new().map_err(|e| Error::Display(e.to_string()))?;
        let event_loop: EventLoop<'static, State> = EventLoop::try_new().map_err(|e| Error::Display(e.to_string()))?;
        let dh = display.handle();
        let compositor_state = CompositorState::new::<State>(&dh);
        let xdg_shell_state = XdgShellState::new::<State>(&dh);
        let xdg_decoration_state = XdgDecorationState::new::<State>(&dh);
        let shm_state = ShmState::new::<State>(&dh, vec![]);
        let outputs = OutputManagerState::new_with_xdg_output::<State>(&dh);
        let viewporter = ViewporterState::new::<State>(&dh);
        let fractional = FractionalScaleManagerState::new::<State>(&dh);
        let cursor_shape = CursorShapeManagerState::new::<State>(&dh);
        let data_device_state = DataDeviceState::new::<State>(&dh);
        let primary_selection_state = PrimarySelectionState::new::<State>(&dh);
        // Only Xwayland sees this global.
        let xwayland_shell_state = XWaylandShellState::new::<State>(&dh);
        let mut seat_state = SeatState::new();
        let mut seat: Seat<State> = seat_state.new_wl_seat(&dh, "meerora");
        let xkb = XkbConfig { layout: &options.keyboard_layout, variant: &options.keyboard_variant, ..Default::default() };
        if let Err(e) = seat.add_keyboard(xkb, 500, 30) {
            tracing::warn!("keyboard layout {:?} failed ({e:?}), using us", options.keyboard_layout);
            seat.add_keyboard(XkbConfig::default(), 500, 30).map_err(|e| Error::Display(format!("no keymap: {e:?}")))?;
        }
        seat.add_pointer();
        let output = Output::new(
            "meerora".into(),
            PhysicalProperties { size: (0, 0).into(), subpixel: Subpixel::Unknown, make: "meerora".into(), model: "agent desktop".into() },
        );
        output.create_global::<State>(&dh);
        let output_size = Size::from((1280, 800));
        let mode = Mode { size: (output_size.w, output_size.h).into(), refresh: 60_000 };
        output.change_current_state(Some(mode), Some(Transform::Normal), Some(Scale::Integer(1)), Some((0, 0).into()));
        output.set_preferred(mode);
        let state = State {
            dh,
            loop_handle: event_loop.handle(),
            compositor_state,
            xdg_shell_state,
            _xdg_decoration_state: xdg_decoration_state,
            shm_state,
            seat_state,
            data_device_state,
            primary_selection_state,
            xwayland_shell_state,
            seat,
            output,
            popups: PopupManager::default(),
            desktops: HashMap::new(),
            windows: Vec::new(),
            x_popups: Vec::new(),
            retired: Vec::new(),
            next_id: 0,
            events: Vec::new(),
            revision: 0,
            scale: 1.0,
            output_size,
            start: Instant::now(),
            clipboard: Clipboard::default(),
            focused: None,
            cursor: CursorState {
                status: CursorImageStatus::default_named(),
                canvas: Default::default(),
                serial: 0,
                dirty: false,
                hotspot: Point::from((0, 0)),
            },
        };
        Ok(Self {
            display,
            event_loop,
            state,
            _viewporter: viewporter,
            _outputs: outputs,
            _fractional: fractional,
            _cursor_shape: cursor_shape,
        })
    }

    /// Creates the desktop of `agent` and returns the socket name of its
    /// user realm for `WAYLAND_DISPLAY`. An existing desktop returns its
    /// name. When another meerora holds the name, a number is added.
    pub fn add_desktop(&mut self, agent: &str) -> Result<String, Error> {
        if let Some(d) = self.state.desktops.get(agent) {
            return Ok(d.name.clone());
        }
        let runtime = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").ok_or(Error::NoRuntimeDir)?);
        let base = format!("meerora-wl-{}", socket_safe(agent));
        let mut last = String::new();
        for n in 0..32 {
            let name = if n == 0 { base.clone() } else { format!("{base}-{n}") };
            match ListeningSocket::bind(&name) {
                Ok(socket) => {
                    let dir = runtime.join(format!("{name}.d"));
                    private_dir(&dir).map_err(|e| Error::Socket(format!("{}: {e}", dir.display())))?;
                    tracing::info!("desktop of {agent} listens on {name}");
                    let place = Place {
                        realm: Realm::User,
                        wayland: name.clone(),
                        socket,
                        clients: Vec::new(),
                        x: None,
                        x_failed: false,
                        bus: None,
                        bus_failed: false,
                    };
                    self.state.desktops.insert(agent.to_string(), Desktop { name: name.clone(), dir, places: vec![place] });
                    return Ok(name);
                }
                Err(e) => last = format!("{name}: {e}"),
            }
        }
        Err(Error::Socket(last))
    }

    /// The socket name of `agent`'s desktop, its user realm.
    pub fn socket(&self, agent: &str) -> Option<&str> {
        self.state.desktops.get(agent).map(|d| d.name.as_str())
    }

    /// The agents that have a desktop.
    pub fn desktops(&self) -> Vec<String> {
        self.state.desktops.keys().cloned().collect()
    }

    /// Removes `agent`'s desktop. Its sockets go away, its clients are
    /// disconnected, so their windows close, and its X servers and buses end.
    pub fn remove_desktop(&mut self, agent: &str) {
        let Some(desktop) = self.state.desktops.remove(agent) else { return };
        let handle = self.display.handle().backend_handle();
        let mut xwms = Vec::new();
        for place in desktop.places {
            for (client, stream) in place.clients {
                handle.kill_client(client, DisconnectReason::ConnectionClosed);
                // The backend closes a killed connection only on its next event, so close it here.
                let _ = stream.shutdown(std::net::Shutdown::Both);
            }
            if let Some(mut x) = place.x {
                if let Some(wm) = x.wm.take() {
                    xwms.push(wm.id());
                    self.state.retired.push(wm);
                }
                // Dropping it disconnects Xwayland, which then ends.
                drop(x);
            }
            drop(place.bus);
        }
        for owner in &mut self.state.clipboard.owners {
            if matches!(owner, Owner::X11(id) if xwms.contains(id)) {
                *owner = Owner::Nobody;
            }
        }
        let gone: Vec<u32> = self.state.windows.iter().filter(|w| w.desktop == agent).map(|w| w.id).collect();
        self.state.windows.retain(|w| w.desktop != agent);
        self.state.x_popups.retain(|p| !gone.contains(&p.parent));
        for id in gone {
            if self.state.focused == Some(id) {
                self.state.focused = None;
            }
            self.state.push(Event::Closed(id));
        }
        // Only meerora's own sockets, configuration and the agent realm's runtime files live here.
        let _ = std::fs::remove_dir_all(&desktop.dir);
        self.flush();
    }

    /// Accepts new clients, handles their requests and the X servers'
    /// events, and flushes the replies. Never blocks.
    pub fn dispatch(&mut self) {
        let mut dh = self.display.handle();
        for (agent, desktop) in &mut self.state.desktops {
            for place in &mut desktop.places {
                loop {
                    match place.socket.accept() {
                        Ok(Some(stream)) => {
                            let data = Arc::new(ClientState {
                                desktop: agent.clone(),
                                realm: place.realm,
                                compositor: CompositorClientState::default(),
                            });
                            let Ok(second) = stream.try_clone() else { continue };
                            match dh.insert_client(stream, data) {
                                Ok(client) => place.clients.push((client.id(), second)),
                                Err(e) => tracing::warn!("cannot accept a client on {}: {e}", place.wayland),
                            }
                        }
                        Ok(None) => break,
                        Err(e) => {
                            tracing::warn!("accepting on {} failed: {e}", place.wayland);
                            break;
                        }
                    }
                }
            }
        }
        if let Err(e) = self.display.dispatch_clients(&mut self.state) {
            tracing::warn!("dispatching Wayland clients failed: {e}");
        }
        self.start_window_managers();
        if let Err(e) = self.event_loop.dispatch(Some(Duration::ZERO), &mut self.state) {
            tracing::warn!("dispatching the X servers failed: {e}");
        }
        // Forget the connections of clients that left.
        let handle = dh.backend_handle();
        for place in self.state.desktops.values_mut().flat_map(|d| d.places.iter_mut()) {
            place.clients.retain(|(id, _)| handle.get_client_data(id.clone()).is_ok());
        }
        self.state.popups.cleanup();
        self.state.x_popups.retain(|p| p.surface.alive());
        self.state.clipboard_step();
        self.hidden_frames();
        self.flush();
    }

    /// Gives every Xwayland that became ready its window manager.
    fn start_window_managers(&mut self) {
        let handle = self.state.loop_handle.clone();
        let mut started = Vec::new();
        for place in self.state.desktops.values_mut().flat_map(|d| d.places.iter_mut()) {
            let Some(x) = place.x.as_mut() else { continue };
            match x.ready() {
                Ok(Some(stream)) => match X11Wm::start_wm(handle.clone(), stream, x.client.clone()) {
                    Ok(wm) => {
                        tracing::info!("X display :{} is ready", x.display);
                        started.push(wm.id());
                        x.wm = Some(wm);
                    }
                    Err(e) => {
                        tracing::warn!("cannot start the X11 window manager on :{}: {e}", x.display);
                        place.x = None;
                        place.x_failed = true;
                    }
                },
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!("Xwayland :{} ended while starting: {e}", x.display);
                    place.x = None;
                    place.x_failed = true;
                }
            }
        }
        // A new X server learns what the selections hold.
        for id in started {
            for target in [SelectionTarget::Clipboard, SelectionTarget::Primary] {
                let mimes = self.state.clipboard.mimes[state::slot(target)].clone();
                if mimes.is_some()
                    && let Some(wm) = self.state.xwm(id)
                {
                    let _ = wm.new_selection(target, mimes);
                }
            }
        }
    }

    /// Frame callbacks for windows whose tile was not drawn for a while.
    fn hidden_frames(&mut self) {
        let time = self.state.start.elapsed();
        let due: Vec<u32> =
            self.state.windows.iter().filter(|w| w.mapped && w.last_frame.elapsed() >= HIDDEN_FRAME_EVERY).map(|w| w.id).collect();
        for id in due {
            self.send_frames(id, time);
        }
    }

    fn send_frames(&mut self, id: u32, time: Duration) {
        let Some(w) = self.state.windows.iter_mut().find(|w| w.id == id) else { return };
        w.last_frame = Instant::now();
        w.window.send_frame(&self.state.output, time, Some(Duration::ZERO), |_, _| None);
        for p in self.state.x_popups.iter().filter(|p| p.parent == id) {
            if let Some(s) = p.surface.wl_surface() {
                smithay::desktop::utils::send_frames_surface_tree(&s, &self.state.output, time, Some(Duration::ZERO), |_, _| None);
            }
        }
    }

    /// Events since the last call.
    pub fn take_events(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.state.events)
    }

    /// Changes whenever a window opens, closes or changes its title.
    pub fn revision(&self) -> u64 {
        self.state.revision
    }

    /// Every shown window, oldest first.
    pub fn windows(&self) -> Vec<WindowInfo> {
        self.state.windows.iter().filter(|w| w.mapped).map(info).collect()
    }

    pub fn window(&self, id: u32) -> Option<WindowInfo> {
        self.state.win(id).filter(|w| w.mapped).map(info)
    }

    /// The latest image of window `id`, composed again only after new
    /// commits, and then only where they changed it.
    pub fn frame(&mut self, id: u32) -> Option<Frame<'_>> {
        let extra = self.menus_of(id);
        let w = self.state.windows.iter_mut().find(|w| w.id == id && w.mapped)?;
        if w.dirty {
            w.dirty = false;
            match compose::compose(&w.window, &extra, &mut w.canvas) {
                Some(rects) if rects.is_empty() => {}
                damage => {
                    w.serial += 1;
                    w.history.push_back((w.serial, damage));
                    while w.history.len() > HISTORY {
                        w.history.pop_front();
                    }
                }
            }
        }
        Some(Frame {
            serial: w.serial,
            width: w.canvas.width,
            height: w.canvas.height,
            scale: w.canvas.scale as f32,
            pixels: &w.canvas.pixels,
            history: &w.history,
        })
    }

    /// The X11 menus of window `id` with their offsets in it.
    fn menus_of(&self, id: u32) -> Vec<(WlSurface, Point<i32, Logical>)> {
        let Some(origin) = self.state.win(id).and_then(|w| w.x11()).map(|x| x.geometry().loc) else { return Vec::new() };
        self.state
            .x_popups
            .iter()
            .filter(|p| p.parent == id)
            .filter_map(|p| Some((p.surface.wl_surface()?, p.surface.geometry().loc - origin)))
            .collect()
    }

    /// Tells the window it was shown. Its frame callbacks fire, so it may
    /// draw the next frame. Windows whose tile is hidden get them only once
    /// a second, so they draw little but keep running.
    pub fn drawn(&mut self, id: u32) {
        let time = self.state.start.elapsed();
        self.send_frames(id, time);
        if let CursorImageStatus::Surface(s) = &self.state.cursor.status {
            smithay::desktop::utils::send_frames_surface_tree(s, &self.state.output, time, Some(Duration::ZERO), |_, _| None);
        }
        self.flush();
    }

    /// Asks the window to take this size in logical units, from its tile.
    pub fn configure(&mut self, id: u32, width: u32, height: u32) {
        let size = Size::from((width.max(1) as i32, height.max(1) as i32));
        let Some(w) = self.state.windows.iter_mut().find(|w| w.id == id) else { return };
        w.want = Some(size);
        if let Some(x) = w.x11() {
            if let Err(e) = x.configure(Rectangle::new((0, 0).into(), size)) {
                tracing::debug!("cannot resize an X11 window: {e}");
            }
        } else if let Some(toplevel) = w.toplevel() {
            toplevel.with_pending_state(|state| state.size = Some(size));
            if toplevel.is_initial_configure_sent() {
                toplevel.send_pending_configure();
            }
        }
        self.flush();
    }

    /// Sends the close request. The window goes away when its client
    /// unmaps it. An X11 client that does not take close requests is
    /// disconnected from its X server.
    pub fn close(&mut self, id: u32) {
        let Some(w) = self.state.win(id) else { return };
        if let Some(x) = w.x11() {
            let _ = x.close();
        } else if let Some(toplevel) = w.toplevel() {
            toplevel.send_close();
        }
        self.flush();
    }

    /// Gives window `id` the keyboard, or nobody with `None`. The window
    /// that loses focus is deactivated and its menus close.
    pub fn focus(&mut self, id: Option<u32>) {
        let id = id.filter(|id| self.state.win(*id).is_some_and(|w| w.mapped));
        if self.state.focused == id {
            return;
        }
        let serial = SERIAL_COUNTER.next_serial();
        let time = self.state.time();
        let keyboard = self.state.seat.get_keyboard().expect("the seat has a keyboard");
        let pointer = self.state.seat.get_pointer().expect("the seat has a pointer");
        // Keys still held go up first, so nothing repeats in the old window.
        for key in keyboard.pressed_keys() {
            keyboard.input::<(), _>(&mut self.state, key, KeyState::Released, serial, time, |_, _, _| FilterResult::Forward);
        }
        keyboard.unset_grab(&mut self.state);
        pointer.unset_grab(&mut self.state, serial, time);
        if let Some(old) = self.state.focused.and_then(|old| self.state.windows.iter().position(|w| w.id == old)) {
            self.state.dismiss_popups(old);
            activate(&self.state.windows[old], false);
        }
        let target = id.and_then(|id| self.state.win(id)).and_then(|w| {
            activate(w, true);
            match w.x11() {
                Some(x) => Some(Focus::X11(x.clone())),
                None => w.surface().map(Focus::Surface),
            }
        });
        if let Some(id) = id {
            self.state.raise_x11(id);
        }
        keyboard.set_focus(&mut self.state, target, serial);
        self.state.focused = id;
        self.flush();
    }

    /// The window with keyboard focus.
    pub fn focused(&self) -> Option<u32> {
        self.state.focused
    }

    /// Moves the pointer to `(x, y)` in window `id`, in logical units from
    /// the top left of its image.
    pub fn pointer_motion(&mut self, id: u32, x: f64, y: f64) {
        let Some(w) = self.state.win(id) else { return };
        let geo = w.window.geometry();
        let location = Point::from((x + f64::from(geo.loc.x), y + f64::from(geo.loc.y)));
        let is_x11 = w.x11().is_some();
        // X11 menus lie over their window, the newest on top.
        let menus = self.menus_of(id);
        let on_menu = menus.iter().rev().find_map(|(s, offset)| under_from_surface_tree(s, location, *offset, WindowSurfaceType::ALL));
        let under =
            on_menu.or_else(|| w.window.surface_under(location, WindowSurfaceType::ALL)).map(|(s, at)| (Focus::Surface(s), at.to_f64()));
        if is_x11 {
            self.state.raise_x11(id);
        }
        let pointer = self.state.seat.get_pointer().expect("the seat has a pointer");
        // A surface the pointer enters sets its own cursor, until then the arrow shows.
        let surface_of = |f: &Focus| f.wl_surface().map(|s| s.into_owned());
        if pointer.current_focus().as_ref().and_then(surface_of) != under.as_ref().and_then(|(f, _)| surface_of(f)) {
            self.state.cursor.status = CursorImageStatus::default_named();
            self.state.cursor.dirty = true;
        }
        let event = MotionEvent { location, serial: SERIAL_COUNTER.next_serial(), time: self.state.time() };
        pointer.motion(&mut self.state, under, &event);
        pointer.frame(&mut self.state);
        self.flush();
    }

    /// The pointer left every window.
    pub fn pointer_leave(&mut self) {
        let pointer = self.state.seat.get_pointer().expect("the seat has a pointer");
        if pointer.current_focus().is_none() {
            return;
        }
        let event = MotionEvent { location: pointer.current_location(), serial: SERIAL_COUNTER.next_serial(), time: self.state.time() };
        pointer.motion(&mut self.state, None, &event);
        pointer.frame(&mut self.state);
        self.state.cursor.status = CursorImageStatus::default_named();
        self.flush();
    }

    /// Presses or releases a Linux button code ([`BTN_LEFT`] and so on).
    /// Call [`Self::pointer_motion`] first, the button goes where the pointer is.
    pub fn pointer_button(&mut self, id: u32, button: u32, pressed: bool) {
        if pressed && self.state.focused != Some(id) {
            self.focus(Some(id));
        }
        let pointer = self.state.seat.get_pointer().expect("the seat has a pointer");
        let state = if pressed { ButtonState::Pressed } else { ButtonState::Released };
        let event = ButtonEvent { serial: SERIAL_COUNTER.next_serial(), time: self.state.time(), button, state };
        pointer.button(&mut self.state, &event);
        pointer.frame(&mut self.state);
        self.flush();
    }

    /// Scrolls by `(dx, dy)` logical pixels, positive is right and down.
    /// `clicks` are wheel clicks, fractions for high resolution wheels, when
    /// the scroll came from a wheel.
    pub fn scroll(&mut self, id: u32, dx: f64, dy: f64, clicks: Option<(f64, f64)>) {
        if self.state.win(id).is_none() {
            return;
        }
        let mut frame = AxisFrame::new(self.state.time()).source(if clicks.is_some() { AxisSource::Wheel } else { AxisSource::Finger });
        for (axis, value, click) in [(Axis::Horizontal, dx, clicks.map(|c| c.0)), (Axis::Vertical, dy, clicks.map(|c| c.1))] {
            if value != 0.0 {
                frame = frame.value(axis, value);
                if let Some(v120) = click.map(|c| (c * 120.0).round() as i32).filter(|v| *v != 0) {
                    frame = frame.v120(axis, v120);
                }
            }
        }
        let pointer = self.state.seat.get_pointer().expect("the seat has a pointer");
        pointer.axis(&mut self.state, frame);
        pointer.frame(&mut self.state);
        self.flush();
    }

    /// Scrolls by wheel clicks, [`WHEEL_STEP`] logical pixels each.
    pub fn scroll_clicks(&mut self, id: u32, clicks_x: f64, clicks_y: f64) {
        self.scroll(id, clicks_x * WHEEL_STEP, clicks_y * WHEEL_STEP, Some((clicks_x, clicks_y)));
    }

    /// Presses or releases a key, as a Linux evdev key code. The window gets
    /// the keyboard first when it has not. Modifiers follow from the keys.
    pub fn key(&mut self, id: u32, keycode: u32, pressed: bool) {
        if self.state.focused != Some(id) {
            self.focus(Some(id));
        }
        if self.state.focused != Some(id) {
            return;
        }
        let keyboard = self.state.seat.get_keyboard().expect("the seat has a keyboard");
        let state = if pressed { KeyState::Pressed } else { KeyState::Released };
        let (serial, time) = (SERIAL_COUNTER.next_serial(), self.state.time());
        keyboard.input::<(), _>(&mut self.state, Keycode::new(keycode + 8), state, serial, time, |_, _, _| FilterResult::Forward);
        self.flush();
    }

    /// Puts text on the clipboard of every desktop, for example the host's
    /// clipboard before a paste. Text the clipboard holds already is left
    /// alone, so a client keeps its own richer selection.
    pub fn set_selection(&mut self, text: &str) {
        if self.state.clipboard.known.as_deref() == Some(text) {
            return;
        }
        self.state.clipboard.known = Some(text.to_string());
        self.offer_text(SelectionTarget::Clipboard, text);
    }

    /// Puts text on the primary selection of every desktop, so a middle
    /// click pastes it.
    pub fn set_primary_selection(&mut self, text: &str) {
        self.state.clipboard.primary = Some(text.to_string());
        self.offer_text(SelectionTarget::Primary, text);
    }

    fn offer_text(&mut self, target: SelectionTarget, text: &str) {
        let i = state::slot(target);
        let text = Arc::new(text.to_string());
        self.state.clipboard.owners[i] = Owner::App;
        self.state.clipboard.texts[i] = Some(text.clone());
        let mimes: Vec<String> = TEXT_MIMES.iter().map(|m| m.to_string()).collect();
        match target {
            SelectionTarget::Clipboard => set_data_device_selection(&self.state.dh, &self.state.seat, mimes.clone(), Offer::Text(text)),
            SelectionTarget::Primary => set_primary_selection(&self.state.dh, &self.state.seat, mimes.clone(), Offer::Text(text)),
        }
        self.state.announce_to_x(target, Some(mimes), None);
        self.flush();
    }

    /// The text of the primary selection as far as the compositor read it.
    pub fn primary_selection(&self) -> Option<&str> {
        self.state.clipboard.primary.as_deref()
    }

    /// The pointer image the client under the pointer asked for.
    pub fn cursor(&mut self) -> Cursor<'_> {
        let cursor = &mut self.state.cursor;
        let surface = match &cursor.status {
            CursorImageStatus::Hidden => return Cursor::Hidden,
            CursorImageStatus::Named(CursorIcon::Default) => return Cursor::Default,
            CursorImageStatus::Named(icon) => return Cursor::Named(icon.name()),
            CursorImageStatus::Surface(s) if !s.alive() => return Cursor::Default,
            CursorImageStatus::Surface(s) => s.clone(),
        };
        if cursor.dirty {
            cursor.dirty = false;
            if compose::compose_surface(&surface, &mut cursor.canvas) {
                cursor.serial += 1;
            } else {
                cursor.canvas.width = 0;
            }
            cursor.hotspot = with_states(&surface, |states| {
                states.data_map.get::<CursorImageSurfaceData>().map(|d| d.lock().unwrap().hotspot).unwrap_or_default()
            });
        }
        if cursor.canvas.width == 0 {
            return Cursor::Hidden;
        }
        Cursor::Image(CursorImage {
            serial: cursor.serial,
            width: cursor.canvas.width,
            height: cursor.canvas.height,
            scale: cursor.canvas.scale as f32,
            hotspot: (cursor.hotspot.x as f32, cursor.hotspot.y as f32),
            pixels: &cursor.canvas.pixels,
        })
    }

    /// Sets the size of the virtual output in logical units and the app's
    /// pixels per point. Clients with `wp_fractional_scale_v1` render at
    /// that scale, the others at the next integer scale.
    pub fn set_output(&mut self, width: u32, height: u32, pixels_per_point: f32) {
        let scale = ((f64::from(pixels_per_point) * 120.0).round() / 120.0).max(1.0);
        let size = Size::from((width.max(64) as i32, height.max(64) as i32));
        if (scale - self.state.scale).abs() < 1e-6 && size == self.state.output_size {
            return;
        }
        let mode =
            Mode { size: ((f64::from(size.w) * scale).round() as i32, (f64::from(size.h) * scale).round() as i32).into(), refresh: 60_000 };
        let output_scale = if scale.fract() == 0.0 { Scale::Integer(scale as i32) } else { Scale::Fractional(scale) };
        self.state.output.change_current_state(Some(mode), None, Some(output_scale), None);
        self.state.output.set_preferred(mode);
        let rescaled = (scale - self.state.scale).abs() >= 1e-6;
        self.state.scale = scale;
        self.state.output_size = size;
        if rescaled {
            self.state.rescale();
        }
        self.flush();
    }

    /// Starts `program` on `agent`'s desktop as a normal program of the
    /// user, with the desktop's Wayland socket, X display and session bus.
    /// The desktop must exist.
    pub fn launch(&mut self, agent: &str, program: &str, args: &[String], cwd: Option<&Path>) -> std::io::Result<Child> {
        self.launch_with(agent, program, args, cwd, &LaunchOptions::default())
    }

    /// [`Self::launch`] with options, for example to force X11.
    pub fn launch_with(
        &mut self,
        agent: &str,
        program: &str,
        args: &[String],
        cwd: Option<&Path>,
        options: &LaunchOptions,
    ) -> std::io::Result<Child> {
        let session = self.session(agent, Realm::User, options.x11)?;
        launch::spawn(launch::command(&session, program, args, cwd, &options.env))
    }

    /// Starts `program` for an agent in its sandbox on its desktop, in the
    /// agent realm. The caller checks that the agent holds `apps.launch`
    /// (D8). `agent_dir` is the agent's directory (D21), which the app sees
    /// read-write as its home and working directory. Nothing else of the
    /// user's home is visible. See [`launch`] for what the sandbox shows.
    pub fn launch_sandboxed(
        &mut self,
        agent: &str,
        agent_dir: &Path,
        program: &str,
        args: &[String],
        options: &SandboxOptions,
    ) -> std::io::Result<Child> {
        if !agent_dir.is_absolute() || !agent_dir.is_dir() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("{} is not an absolute path to a directory", agent_dir.display()),
            ));
        }
        let session = self.session(agent, Realm::Agent, options.x11)?;
        let desktop = self.state.desktops.get(agent).expect("session made it");
        let run_dir = desktop.agent_run_dir();
        let x_socket = desktop.place(Realm::Agent).and_then(|p| p.x.as_ref()).filter(|_| session.display.is_some()).map(XServer::socket);
        let plan = launch::SandboxPlan { agent_dir, run_dir: &run_dir, x_socket, options };
        let cmd = launch::sandboxed(&session, &plan, program, args)?;
        tracing::debug!("starting {program} for {agent} in its sandbox: {:?}", cmd.get_args().collect::<Vec<_>>());
        launch::spawn(cmd)
    }

    /// Starts the X server of a realm of `agent`'s desktop unless it runs,
    /// and returns its display number. Launches do this by themselves.
    pub fn ensure_x11(&mut self, agent: &str, realm: Realm) -> Option<u32> {
        if realm == Realm::Agent {
            self.agent_place(agent).ok()?;
        }
        let dh = self.display.handle();
        let desktop = self.state.desktops.get_mut(agent)?;
        let wrapper = (realm == Realm::Agent).then(|| desktop.dir.join("bin"));
        let place = desktop.place_mut(realm)?;
        if let Some(x) = &place.x {
            return Some(x.display);
        }
        if place.x_failed {
            return None;
        }
        match XServer::spawn(&dh, XOwner { agent: agent.to_string(), realm }, wrapper.as_deref()) {
            Ok(x) => {
                tracing::info!("X display :{} starts for the {realm:?} realm of {agent}", x.display);
                let n = x.display;
                place.x = Some(x);
                Some(n)
            }
            Err(e) => {
                tracing::warn!("no X display for {agent}: {e}");
                place.x_failed = true;
                None
            }
        }
    }

    /// The display number of a realm's X server, once it was started.
    pub fn x11_display(&self, agent: &str, realm: Realm) -> Option<u32> {
        self.state.desktops.get(agent)?.place(realm)?.x.as_ref().map(|x| x.display)
    }

    /// The X server of a realm is ready for clients.
    pub fn x11_ready(&self, agent: &str, realm: Realm) -> bool {
        self.state.desktops.get(agent).and_then(|d| d.place(realm)).and_then(|p| p.x.as_ref()).is_some_and(|x| x.wm.is_some())
    }

    /// `DBUS_SESSION_BUS_ADDRESS` of a realm's bus, once it was started.
    pub fn bus_address(&self, agent: &str, realm: Realm) -> Option<String> {
        let bus = self.state.desktops.get(agent)?.place(realm)?.bus.as_ref()?;
        Some(bus::Bus::address(&bus.socket))
    }

    /// What a program of `realm` connects to, with the realm's X server and
    /// bus started on first use.
    fn session(&mut self, agent: &str, realm: Realm, x11: X11) -> std::io::Result<launch::Session> {
        let missing = || std::io::Error::new(std::io::ErrorKind::NotFound, format!("no desktop for {agent}"));
        if realm == Realm::Agent {
            self.agent_place(agent)?;
        }
        let display = match x11 {
            X11::Off => None,
            _ => self.ensure_x11(agent, realm),
        };
        let desktop = self.state.desktops.get_mut(agent).ok_or_else(missing)?;
        let (socket, config) = match realm {
            Realm::User => (desktop.dir.join("bus"), desktop.dir.join("bus.conf")),
            Realm::Agent => (desktop.agent_run_dir().join("bus"), desktop.dir.join("agent-bus.conf")),
        };
        let place = desktop.place_mut(realm).ok_or_else(missing)?;
        if place.bus.as_mut().is_some_and(|b| !b.alive()) {
            place.bus = None;
        }
        if place.bus.is_none() && !place.bus_failed {
            match bus::Bus::start(&socket, &config) {
                Ok(bus) => place.bus = Some(bus),
                Err(e) => {
                    tracing::warn!("no session bus for the {realm:?} realm of {agent}: {e}");
                    place.bus_failed = true;
                }
            }
        }
        let bus = place.bus.as_ref().map(|b| bus::Bus::address(&b.socket));
        Ok(launch::Session { wayland: place.wayland.clone(), display, bus, x11 })
    }

    /// Creates the agent realm of `agent`'s desktop with its runtime
    /// directory and its Wayland socket in there.
    fn agent_place(&mut self, agent: &str) -> std::io::Result<()> {
        let desktop = self
            .state
            .desktops
            .get_mut(agent)
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, format!("no desktop for {agent}")))?;
        if desktop.place(Realm::Agent).is_some() {
            return Ok(());
        }
        let run = desktop.agent_run_dir();
        private_dir(&run)?;
        let path = run.join("wayland-0");
        let socket = ListeningSocket::bind_absolute(path.clone()).map_err(|e| std::io::Error::other(format!("{}: {e}", path.display())))?;
        tracing::info!("the agent realm of {agent} listens on {}", path.display());
        desktop.places.push(Place {
            realm: Realm::Agent,
            wayland: "wayland-0".into(),
            socket,
            clients: Vec::new(),
            x: None,
            x_failed: false,
            bus: None,
            bus_failed: false,
        });
        Ok(())
    }

    fn flush(&mut self) {
        if let Err(e) = self.display.flush_clients() {
            tracing::debug!("flushing Wayland clients failed: {e}");
        }
    }
}

impl Drop for Compositor {
    /// Ends the X servers and buses and removes the runtime directories.
    fn drop(&mut self) {
        for agent in self.desktops() {
            self.remove_desktop(&agent);
        }
    }
}

/// Sets the activated state of a toplevel, Wayland or X11.
fn activate(w: &state::Win, on: bool) {
    if let Some(x) = w.x11() {
        let _ = x.set_activated(on);
    } else if let Some(toplevel) = w.toplevel() {
        toplevel.with_pending_state(|s| {
            if on {
                s.states.set(xdg_toplevel::State::Activated);
            } else {
                s.states.unset(xdg_toplevel::State::Activated);
            }
        });
        toplevel.send_pending_configure();
    }
}

fn info(w: &state::Win) -> WindowInfo {
    let geo = w.window.geometry();
    WindowInfo {
        id: w.id,
        desktop: w.desktop.clone(),
        title: w.title.clone(),
        app_id: w.app_id.clone(),
        size: (geo.size.w.max(0) as u32, geo.size.h.max(0) as u32),
        x11: w.x11().is_some(),
        sandboxed: w.realm == Realm::Agent,
    }
}

/// Creates a directory that only the user can enter.
fn private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
}

/// An agent id as part of a socket name: lowercase letters, digits, `-` and `_`.
fn socket_safe(agent: &str) -> String {
    let s: String = agent
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c.to_ascii_lowercase() } else { '_' })
        .take(40)
        .collect();
    if s.is_empty() { "agent".into() } else { s }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_names_are_safe() {
        assert_eq!(socket_safe("ember"), "ember");
        assert_eq!(socket_safe("Mira.Example/x"), "mira_example_x");
        assert_eq!(socket_safe(""), "agent");
    }

    #[test]
    fn damage_adds_up_over_compositions() {
        let history: VecDeque<(u64, Option<Vec<Rect>>)> =
            [(3, Some(vec![[0, 0, 1, 1]])), (4, Some(vec![[5, 5, 2, 2]])), (5, None), (6, Some(vec![]))].into_iter().collect();
        let frame = |serial| Frame { serial, width: 10, height: 10, scale: 1.0, pixels: &[], history: &history };
        assert_eq!(frame(4).damage_since(2), Some(vec![[0, 0, 1, 1], [5, 5, 2, 2]]));
        assert_eq!(frame(4).damage_since(4), Some(vec![]));
        assert_eq!(frame(5).damage_since(4), None, "a whole new image");
        assert_eq!(frame(6).damage_since(5), Some(vec![]));
        assert_eq!(frame(4).damage_since(1), None, "older than the history");
    }
}
