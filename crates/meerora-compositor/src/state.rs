//! The compositor state and the smithay protocol handlers: wl_compositor,
//! wl_subcompositor, wl_shm, xdg_shell with popups, xdg_decoration,
//! wl_seat, wl_data_device, the primary selection, wl_output,
//! wp_viewporter, wp_fractional_scale_v1, wp_cursor_shape_v1 and the
//! selection bridge to X11. The X11 window manager is in xwayland.rs.
//! Anvil, the reference compositor in the Smithay repository, was the
//! model for the handlers.

use std::collections::{HashMap, VecDeque};
use std::io::{Read as _, Write as _};
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use smithay::backend::renderer::utils::{on_commit_buffer_handler, with_renderer_surface_state};
use smithay::desktop::{
    PopupKeyboardGrab, PopupKind, PopupManager, PopupPointerGrab, PopupUngrabStrategy, Window, find_popup_root_surface,
    get_popup_toplevel_coords,
};
use smithay::input::pointer::{CursorImageStatus, Focus as GrabFocus};
use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::output::Output;
use smithay::reexports::calloop::LoopHandle;
use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode as DecorationMode;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::backend::{ClientData, ClientId, DisconnectReason};
use smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer;
use smithay::reexports::wayland_server::protocol::wl_output::WlOutput;
use smithay::reexports::wayland_server::protocol::wl_seat::WlSeat;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{Client, DisplayHandle, ListeningSocket, Resource};
use smithay::utils::{Logical, Point, Rectangle, Serial, Size, Transform};
use smithay::wayland::buffer::BufferHandler;
use smithay::wayland::compositor::{
    CompositorClientState, CompositorHandler, CompositorState, TraversalAction, get_parent, is_sync_subsurface, send_surface_state,
    with_states, with_surface_tree_downward,
};
use smithay::wayland::fractional_scale::{FractionalScaleHandler, with_fractional_scale};
use smithay::wayland::output::OutputHandler;
use smithay::wayland::seat::WaylandFocus;
use smithay::wayland::selection::data_device::{
    ClientDndGrabHandler, DataDeviceHandler, DataDeviceState, ServerDndGrabHandler, request_data_device_client_selection,
    set_data_device_focus,
};
use smithay::wayland::selection::primary_selection::{
    PrimarySelectionHandler, PrimarySelectionState, request_primary_client_selection, set_primary_focus,
};
use smithay::wayland::selection::{SelectionHandler, SelectionSource, SelectionTarget};
use smithay::wayland::shell::xdg::decoration::{XdgDecorationHandler, XdgDecorationState};
use smithay::wayland::shell::xdg::{
    PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState, XdgToplevelSurfaceData,
};
use smithay::wayland::shm::{ShmHandler, ShmState};
use smithay::wayland::tablet_manager::TabletSeatHandler;
use smithay::wayland::xwayland_shell::XWaylandShellState;
use smithay::xwayland::xwm::XwmId;
use smithay::xwayland::{X11Surface, X11Wm, XWaylandClientData};
use smithay::{
    delegate_compositor, delegate_cursor_shape, delegate_data_device, delegate_fractional_scale, delegate_output,
    delegate_primary_selection, delegate_seat, delegate_shm, delegate_viewporter, delegate_xdg_decoration, delegate_xdg_shell,
    delegate_xwayland_shell,
};

use crate::bus::Bus;
use crate::compose::{Canvas, Rect};
use crate::focus::Focus;
use crate::xwayland::{XOwner, XServer};
use crate::{Event, Realm};

/// Text formats offered and read on the clipboard, best first.
pub(crate) const TEXT_MIMES: [&str; 5] = ["text/plain;charset=utf-8", "text/plain", "UTF8_STRING", "STRING", "TEXT"];
/// Selection text above this size is not read.
const MAX_SELECTION: usize = 4 << 20;
/// A client that does not finish writing a selection in this time is skipped.
const SELECTION_TIMEOUT: Duration = Duration::from_secs(3);
/// How many compositions of a window keep their damage, for partial uploads.
pub(crate) const HISTORY: usize = 16;

/// Every Wayland client's data holds the desktop and realm whose socket it came through.
pub(crate) struct ClientState {
    pub desktop: String,
    pub realm: Realm,
    pub compositor: CompositorClientState,
}

impl ClientData for ClientState {
    fn initialized(&self, _client: ClientId) {}
    fn disconnected(&self, _client: ClientId, _reason: DisconnectReason) {}
}

/// One realm of a desktop: its Wayland socket with the clients that came
/// through it, its X server and its session bus. The X server and the bus
/// start with the first program of the realm.
pub(crate) struct Place {
    pub realm: Realm,
    /// `WAYLAND_DISPLAY` for programs of this realm, as they see it.
    pub wayland: String,
    pub socket: ListeningSocket,
    /// Each client with a second handle on its connection, so removing the
    /// desktop can shut the connection at once.
    pub clients: Vec<(ClientId, UnixStream)>,
    pub x: Option<XServer>,
    /// The X server could not start, so programs start without one.
    pub x_failed: bool,
    pub bus: Option<Bus>,
    pub bus_failed: bool,
}

/// One agent desktop.
pub(crate) struct Desktop {
    /// The socket name of the user realm in `$XDG_RUNTIME_DIR`.
    pub name: String,
    /// `$XDG_RUNTIME_DIR/<name>.d`: the buses, their configuration and the
    /// runtime directory of the agent realm. It goes with the desktop.
    pub dir: PathBuf,
    pub places: Vec<Place>,
}

impl Desktop {
    pub fn place(&self, realm: Realm) -> Option<&Place> {
        self.places.iter().find(|p| p.realm == realm)
    }

    pub fn place_mut(&mut self, realm: Realm) -> Option<&mut Place> {
        self.places.iter_mut().find(|p| p.realm == realm)
    }

    /// The runtime directory that sandboxed programs see as `XDG_RUNTIME_DIR`.
    pub fn agent_run_dir(&self) -> PathBuf {
        self.dir.join("agent")
    }
}

/// A toplevel window, Wayland or X11, and its composed image.
pub(crate) struct Win {
    pub id: u32,
    pub desktop: String,
    pub realm: Realm,
    pub window: Window,
    /// It has a buffer, so the app shows it.
    pub mapped: bool,
    /// Something in it committed since the last composition.
    pub dirty: bool,
    pub canvas: Canvas,
    pub serial: u64,
    /// The damage of the last compositions, oldest first. Each entry holds
    /// the serial a composition made and what changed against the one
    /// before, `None` for a whole new image.
    pub history: VecDeque<(u64, Option<Vec<Rect>>)>,
    pub title: String,
    pub app_id: String,
    /// The size its tile asked for, in logical units.
    pub want: Option<Size<i32, Logical>>,
    /// When it last got frame callbacks.
    pub last_frame: Instant,
}

impl Win {
    pub fn new(id: u32, desktop: String, realm: Realm, window: Window) -> Self {
        Self {
            id,
            desktop,
            realm,
            window,
            mapped: false,
            dirty: true,
            canvas: Canvas::default(),
            serial: 0,
            history: VecDeque::new(),
            title: String::new(),
            app_id: String::new(),
            want: None,
            last_frame: Instant::now(),
        }
    }

    pub fn surface(&self) -> Option<WlSurface> {
        self.window.wl_surface().map(|s| s.into_owned())
    }

    pub fn toplevel(&self) -> Option<&ToplevelSurface> {
        self.window.toplevel()
    }

    pub fn x11(&self) -> Option<&X11Surface> {
        self.window.x11_surface()
    }

    pub fn xwm(&self) -> Option<XwmId> {
        self.x11().and_then(X11Surface::xwm_id)
    }
}

/// An X11 override-redirect window, a menu or a tooltip, drawn in the tile
/// of the window it belongs to.
pub(crate) struct XPopup {
    pub surface: X11Surface,
    pub parent: u32,
}

/// Who holds a selection, so that reads go to the right place.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum Owner {
    #[default]
    Nobody,
    /// A Wayland client.
    Client,
    /// The app, from the host clipboard.
    App,
    /// A client of this X server.
    X11(XwmId),
}

/// What a selection that the compositor sets carries.
#[derive(Clone, Debug)]
pub(crate) enum Offer {
    Text(Arc<String>),
    X11(XwmId),
}

/// Index of a selection in the arrays of [`Clipboard`].
pub(crate) fn slot(target: SelectionTarget) -> usize {
    match target {
        SelectionTarget::Clipboard => 0,
        SelectionTarget::Primary => 1,
    }
}

/// A selection read in progress. The client writes into `fd`.
pub(crate) struct Reader {
    fd: OwnedFd,
    data: Vec<u8>,
    since: Instant,
    target: SelectionTarget,
}

#[derive(Default)]
pub(crate) struct Clipboard {
    /// Selections to read once dispatch returned, with the format to read.
    pub requests: Vec<(SelectionTarget, String)>,
    pub readers: Vec<Reader>,
    /// The clipboard text as far as we know, from a client or from the app.
    pub known: Option<String>,
    /// The text of the primary selection as far as we know.
    pub primary: Option<String>,
    /// Who holds the clipboard and the primary selection, by [`slot`].
    pub owners: [Owner; 2],
    /// What the app put on them, by [`slot`].
    pub texts: [Option<Arc<String>>; 2],
    /// The formats they offer, by [`slot`], for X servers that start later.
    pub mimes: [Option<Vec<String>>; 2],
}

/// The cursor that clients asked for.
pub(crate) struct CursorState {
    pub status: CursorImageStatus,
    /// The composed image of a surface cursor.
    pub canvas: Canvas,
    pub serial: u64,
    pub dirty: bool,
    pub hotspot: Point<i32, Logical>,
}

pub(crate) struct State {
    pub dh: DisplayHandle,
    pub loop_handle: LoopHandle<'static, State>,
    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    /// Kept for the lifetime of the global.
    pub _xdg_decoration_state: XdgDecorationState,
    pub shm_state: ShmState,
    pub seat_state: SeatState<State>,
    pub data_device_state: DataDeviceState,
    pub primary_selection_state: PrimarySelectionState,
    pub xwayland_shell_state: XWaylandShellState,
    pub seat: Seat<State>,
    pub output: Output,
    pub popups: PopupManager,
    pub desktops: HashMap<String, Desktop>,
    pub windows: Vec<Win>,
    pub x_popups: Vec<XPopup>,
    /// Window managers of removed desktops. They stay until their X
    /// connection closed, because events of theirs may still be queued.
    pub retired: Vec<X11Wm>,
    pub next_id: u32,
    pub events: Vec<Event>,
    pub revision: u64,
    /// The output scale, the app's pixels per point. It may be fractional.
    pub scale: f64,
    /// Logical size of the virtual output.
    pub output_size: Size<i32, Logical>,
    pub start: Instant,
    pub clipboard: Clipboard,
    /// The window with keyboard focus.
    pub focused: Option<u32>,
    pub cursor: CursorState,
}

impl State {
    pub fn time(&self) -> u32 {
        self.start.elapsed().as_millis() as u32
    }

    pub fn win(&self, id: u32) -> Option<&Win> {
        self.windows.iter().find(|w| w.id == id)
    }

    /// The window whose toplevel surface is `root`.
    pub fn window_index(&self, root: &WlSurface) -> Option<usize> {
        self.windows.iter().position(|w| w.window.wl_surface().as_deref() == Some(root))
    }

    pub fn x_window_index(&self, surface: &X11Surface) -> Option<usize> {
        self.windows.iter().position(|w| w.x11() == Some(surface))
    }

    pub fn push(&mut self, event: Event) {
        self.revision += 1;
        self.events.push(event);
    }

    /// The integer buffer scale that clients without fractional scaling use.
    pub fn integer_scale(&self) -> i32 {
        (self.scale - 0.01).ceil().max(1.0) as i32
    }

    /// Tells every surface of the tree about the one output and its scale.
    pub fn enter_output(&self, surface: &WlSurface) {
        let (output, scale, fraction) = (&self.output, self.integer_scale(), self.scale);
        with_surface_tree_downward(
            surface,
            (),
            |_, _, _| TraversalAction::DoChildren(()),
            |s, states, _| {
                output.enter(s);
                send_surface_state(s, states, scale, Transform::Normal);
                with_fractional_scale(states, |fs| fs.set_preferred_scale(fraction));
            },
            |_, _, _| true,
        );
    }

    /// Sends the new scale to every surface and recomposes every window.
    pub fn rescale(&mut self) {
        for w in &mut self.windows {
            w.dirty = true;
        }
        let mut surfaces: Vec<WlSurface> = self.windows.iter().filter_map(Win::surface).collect();
        surfaces.extend(self.x_popups.iter().filter_map(|p| p.surface.wl_surface()));
        if let CursorImageStatus::Surface(s) = &self.cursor.status {
            surfaces.push(s.clone());
        }
        for s in surfaces {
            self.enter_output(&s);
            for (popup, _) in PopupManager::popups_for_surface(&s) {
                self.enter_output(popup.wl_surface());
            }
        }
    }

    /// Fits a popup into its toplevel's window geometry, so a menu stays inside the tile.
    fn unconstrain_popup(&self, popup: &PopupSurface) {
        let kind = PopupKind::Xdg(popup.clone());
        let Ok(root) = find_popup_root_surface(&kind) else { return };
        let Some(i) = self.window_index(&root) else { return };
        let geometry = self.windows[i].window.geometry();
        // The positioner wants the target relative to the parent's window geometry.
        let mut target = Rectangle::from_size(geometry.size);
        target.loc -= get_popup_toplevel_coords(&kind);
        popup.with_pending_state(|state| {
            state.geometry = state.positioner.get_unconstrained_geometry(target);
        });
    }

    /// Closes every popup of window `i`, for example when it loses focus.
    /// X11 menus belong to their client, which closes them itself.
    pub fn dismiss_popups(&mut self, i: usize) {
        let Some(root) = self.windows[i].toplevel().map(|t| t.wl_surface().clone()) else { return };
        for (popup, _) in PopupManager::popups_for_surface(&root) {
            if let PopupKind::Xdg(p) = &popup {
                p.send_popup_done();
            }
            let _ = PopupManager::dismiss_popup(&root, &popup);
        }
        self.windows[i].dirty = true;
    }

    /// Marks window `id` for a new composition.
    pub fn touch(&mut self, id: u32) {
        if let Some(w) = self.windows.iter_mut().find(|w| w.id == id) {
            w.dirty = true;
        }
    }

    /// Window `i` shows once its surface has a buffer, and goes when it has none.
    pub fn check_mapped(&mut self, i: usize) {
        let w = &mut self.windows[i];
        let mapped = w.surface().and_then(|s| with_renderer_surface_state(&s, |s| s.buffer().is_some())).unwrap_or(false);
        if mapped != w.mapped {
            w.mapped = mapped;
            let id = w.id;
            self.update_title(i);
            self.push(if mapped { Event::Opened(id) } else { Event::Closed(id) });
        }
    }

    pub fn update_title(&mut self, i: usize) {
        let w = &self.windows[i];
        let (title, app_id) = if let Some(x) = w.x11() {
            (x.title(), x.class())
        } else if let Some(toplevel) = w.toplevel() {
            with_states(toplevel.wl_surface(), |states| {
                let data = states.data_map.get::<XdgToplevelSurfaceData>().map(|d| d.lock().unwrap());
                data.map(|d| (d.title.clone().unwrap_or_default(), d.app_id.clone().unwrap_or_default())).unwrap_or_default()
            })
        } else {
            return;
        };
        let w = &mut self.windows[i];
        if w.title != title || w.app_id != app_id {
            w.title = title;
            w.app_id = app_id;
            if w.mapped {
                let id = w.id;
                self.push(Event::Changed(id));
            }
        }
    }

    /// The X window manager with this id, wherever it runs.
    pub fn xwm(&mut self, id: XwmId) -> Option<&mut X11Wm> {
        self.desktops
            .values_mut()
            .flat_map(|d| d.places.iter_mut())
            .filter_map(|p| p.x.as_mut())
            .filter_map(|x| x.wm.as_mut())
            .chain(self.retired.iter_mut())
            .find(|wm| wm.id() == id)
    }

    /// Every running X window manager.
    pub fn xwm_ids(&self) -> Vec<XwmId> {
        self.desktops
            .values()
            .flat_map(|d| d.places.iter())
            .filter_map(|p| p.x.as_ref())
            .filter_map(|x| x.wm.as_ref())
            .map(X11Wm::id)
            .collect()
    }

    /// The desktop and realm of an X window manager.
    pub fn xwm_owner(&self, id: XwmId) -> Option<(String, Realm)> {
        self.desktops.iter().find_map(|(agent, d)| {
            d.places
                .iter()
                .find(|p| p.x.as_ref().and_then(|x| x.wm.as_ref()).is_some_and(|wm| wm.id() == id))
                .map(|p| (agent.clone(), p.realm))
        })
    }

    /// The X server whose window has the keyboard.
    pub fn focused_xwm(&self) -> Option<XwmId> {
        self.focused.and_then(|id| self.win(id)).and_then(Win::xwm)
    }

    /// Asks X server `id` for its selection, written into `fd`.
    pub fn read_from_x(&mut self, id: XwmId, target: SelectionTarget, mime: String, fd: OwnedFd) {
        let handle = self.loop_handle.clone();
        if let Some(wm) = self.xwm(id)
            && let Err(e) = wm.send_selection(target, mime, fd, handle)
        {
            tracing::debug!("cannot read the X selection: {e}");
        }
    }

    /// Tells every X server but `except` that the selection changed, so its
    /// clients can paste it. Whether they may read it is decided per read.
    pub fn announce_to_x(&mut self, target: SelectionTarget, mimes: Option<Vec<String>>, except: Option<XwmId>) {
        self.clipboard.mimes[slot(target)] = mimes.clone();
        for id in self.xwm_ids().into_iter().filter(|id| Some(*id) != except) {
            if let Some(wm) = self.xwm(id)
                && let Err(e) = wm.new_selection(target, mimes.clone())
            {
                tracing::debug!("cannot offer the selection to X: {e}");
            }
        }
    }

    /// Reads a selection in a text format, once dispatch returned.
    pub fn read_selection(&mut self, target: SelectionTarget, offered: &[String]) {
        if let Some(mime) = TEXT_MIMES.iter().find(|m| offered.iter().any(|o| o == *m)) {
            self.clipboard.requests.retain(|(t, _)| *t != target);
            self.clipboard.requests.push((target, mime.to_string()));
        }
    }

    /// Hands the current selection to a client of X server `to`, which does
    /// not own it. Nothing happens when it cannot be served.
    pub fn serve_to_x(&mut self, to: XwmId, target: SelectionTarget, mime: String, fd: OwnedFd) {
        match self.clipboard.owners[slot(target)].clone() {
            Owner::Client => {
                let result = match target {
                    SelectionTarget::Clipboard => request_data_device_client_selection(&self.seat, mime, fd).map_err(|e| e.to_string()),
                    SelectionTarget::Primary => request_primary_client_selection(&self.seat, mime, fd).map_err(|e| e.to_string()),
                };
                if let Err(e) = result {
                    tracing::debug!("cannot hand the selection to X: {e}");
                }
            }
            Owner::App => {
                if let Some(text) = self.clipboard.texts[slot(target)].clone() {
                    write_later(fd, text);
                }
            }
            Owner::X11(from) if from != to => self.read_from_x(from, target, mime, fd),
            _ => {}
        }
    }

    /// Reads the selections that changed and collects what the clients
    /// wrote so far.
    pub fn clipboard_step(&mut self) {
        for (target, mime) in std::mem::take(&mut self.clipboard.requests) {
            let mut fds = [0; 2];
            // SAFETY: pipe2 fills two descriptors that we own from here on.
            if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } != 0 {
                continue;
            }
            let (read, write) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
            let started = match self.clipboard.owners[slot(target)].clone() {
                Owner::Client => match target {
                    SelectionTarget::Clipboard => request_data_device_client_selection(&self.seat, mime, write).map_err(|e| e.to_string()),
                    SelectionTarget::Primary => request_primary_client_selection(&self.seat, mime, write).map_err(|e| e.to_string()),
                },
                Owner::X11(id) => {
                    self.read_from_x(id, target, mime, write);
                    Ok(())
                }
                _ => Err("nobody holds it".to_string()),
            };
            match started {
                Ok(()) => self.clipboard.readers.push(Reader { fd: read, data: Vec::new(), since: Instant::now(), target }),
                Err(e) => tracing::debug!("cannot read the selection: {e}"),
            }
        }
        let mut done = Vec::new();
        self.clipboard.readers.retain_mut(|r| {
            let mut file = std::mem::ManuallyDrop::new(unsafe { std::fs::File::from_raw_fd(r.fd.as_raw_fd()) });
            let mut chunk = [0u8; 65536];
            loop {
                match file.read(&mut chunk) {
                    Ok(0) => {
                        done.push((r.target, std::mem::take(&mut r.data)));
                        return false;
                    }
                    Ok(n) if r.data.len() + n <= MAX_SELECTION => r.data.extend_from_slice(&chunk[..n]),
                    Ok(_) => return false,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return r.since.elapsed() < SELECTION_TIMEOUT,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(_) => return false,
                }
            }
        });
        for (target, data) in done {
            let Ok(text) = String::from_utf8(data) else { continue };
            if text.is_empty() {
                continue;
            }
            match target {
                SelectionTarget::Clipboard if self.clipboard.known.as_deref() != Some(text.as_str()) => {
                    self.clipboard.known = Some(text.clone());
                    self.push(Event::Selection(text));
                }
                SelectionTarget::Primary if self.clipboard.primary.as_deref() != Some(text.as_str()) => {
                    self.clipboard.primary = Some(text.clone());
                    self.push(Event::PrimarySelection(text));
                }
                _ => {}
            }
        }
    }
}

/// Writes `text` into a selection pipe on a thread, so a client that reads
/// slowly cannot stall the app.
pub(crate) fn write_later(fd: OwnedFd, text: Arc<String>) {
    std::thread::spawn(move || {
        let mut file = std::fs::File::from(fd);
        let _ = file.write_all(text.as_bytes());
    });
}

/// The desktop and realm of the client that owns `surface`.
pub(crate) fn desktop_of(surface: &WlSurface) -> (String, Realm) {
    let Some(client) = surface.client() else { return (String::new(), Realm::User) };
    if let Some(data) = client.get_data::<ClientState>() {
        return (data.desktop.clone(), data.realm);
    }
    if let Some(owner) = client.get_data::<XWaylandClientData>().and_then(|d| d.user_data().get::<XOwner>().cloned()) {
        return (owner.agent, owner.realm);
    }
    (String::new(), Realm::User)
}

impl BufferHandler for State {
    fn buffer_destroyed(&mut self, _buffer: &WlBuffer) {}
}

impl CompositorHandler for State {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        if let Some(data) = client.get_data::<ClientState>() {
            return &data.compositor;
        }
        &client.get_data::<XWaylandClientData>().expect("every client comes through a desktop socket or is Xwayland").compositor_state
    }

    fn commit(&mut self, surface: &WlSurface) {
        on_commit_buffer_handler::<Self>(surface);
        self.popups.commit(surface);
        if is_sync_subsurface(surface) {
            // Applied with the parent's next commit.
            return;
        }
        let mut root = surface.clone();
        while let Some(parent) = get_parent(&root) {
            root = parent;
        }
        if let Some(i) = self.window_index(&root) {
            let w = &mut self.windows[i];
            w.window.on_commit();
            w.dirty = true;
            if &root == surface {
                if let Some(toplevel) = w.toplevel()
                    && !toplevel.is_initial_configure_sent()
                {
                    toplevel.send_configure();
                }
                self.check_mapped(i);
            }
        } else if let Some(popup) = self.popups.find_popup(&root) {
            if let PopupKind::Xdg(p) = &popup
                && !p.is_initial_configure_sent()
                && let Err(e) = p.send_configure()
            {
                tracing::debug!("initial popup configure failed: {e}");
            }
            if let Ok(top) = find_popup_root_surface(&popup)
                && let Some(i) = self.window_index(&top)
            {
                self.windows[i].dirty = true;
            }
        } else if let Some(parent) = self.x_popups.iter().find(|p| p.surface.wl_surface().as_ref() == Some(&root)).map(|p| p.parent) {
            self.touch(parent);
        } else if matches!(&self.cursor.status, CursorImageStatus::Surface(s) if *s == root) {
            self.cursor.dirty = true;
        }
        self.enter_output(surface);
    }
}

impl XdgShellHandler for State {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        // The tile head is the title bar, so decorations are the server's.
        // Tiled tells toolkits to leave out shadows and rounded corners.
        let bounds = self.output_size;
        surface.with_pending_state(|state| {
            state.states.set(xdg_toplevel::State::TiledLeft);
            state.states.set(xdg_toplevel::State::TiledRight);
            state.states.set(xdg_toplevel::State::TiledTop);
            state.states.set(xdg_toplevel::State::TiledBottom);
            state.decoration_mode = Some(DecorationMode::ServerSide);
            state.bounds = Some(bounds);
        });
        self.next_id += 1;
        let (desktop, realm) = desktop_of(surface.wl_surface());
        self.windows.push(Win::new(self.next_id, desktop, realm, Window::new_wayland_window(surface)));
    }

    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        self.unconstrain_popup(&surface);
        if let Err(e) = self.popups.track_popup(PopupKind::from(surface)) {
            tracing::warn!("cannot track a popup: {e}");
        }
    }

    fn reposition_request(&mut self, surface: PopupSurface, positioner: PositionerState, token: u32) {
        surface.with_pending_state(|state| {
            state.geometry = positioner.get_geometry();
            state.positioner = positioner;
        });
        self.unconstrain_popup(&surface);
        surface.send_repositioned(token);
    }

    fn grab(&mut self, surface: PopupSurface, seat: WlSeat, serial: Serial) {
        let Some(seat) = Seat::<State>::from_resource(&seat) else { return };
        let kind = PopupKind::Xdg(surface);
        let Ok(root) = find_popup_root_surface(&kind) else { return };
        if self.window_index(&root).is_none() {
            return;
        }
        let Ok(mut grab) = self.popups.grab_popup(Focus::Surface(root), kind, &seat, serial) else { return };
        if let Some(keyboard) = seat.get_keyboard() {
            if keyboard.is_grabbed() && !(keyboard.has_grab(serial) || keyboard.has_grab(grab.previous_serial().unwrap_or(serial))) {
                grab.ungrab(PopupUngrabStrategy::All);
                return;
            }
            keyboard.set_focus(self, grab.current_grab(), serial);
            keyboard.set_grab(self, PopupKeyboardGrab::new(&grab), serial);
        }
        if let Some(pointer) = seat.get_pointer() {
            if pointer.is_grabbed()
                && !(pointer.has_grab(serial) || pointer.has_grab(grab.previous_serial().unwrap_or_else(|| grab.serial())))
            {
                grab.ungrab(PopupUngrabStrategy::All);
                return;
            }
            pointer.set_grab(self, PopupPointerGrab::new(&grab), serial, GrabFocus::Keep);
        }
    }

    fn maximize_request(&mut self, surface: ToplevelSurface) {
        // A tile has its size. The protocol still wants an answer.
        if surface.is_initial_configure_sent() {
            surface.send_configure();
        }
    }

    fn fullscreen_request(&mut self, surface: ToplevelSurface, _output: Option<WlOutput>) {
        if surface.is_initial_configure_sent() {
            surface.send_configure();
        }
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        let Some(i) = self.window_index(surface.wl_surface()) else { return };
        let w = self.windows.remove(i);
        if self.focused == Some(w.id) {
            self.focused = None;
        }
        if w.mapped {
            self.push(Event::Closed(w.id));
        }
    }

    fn popup_destroyed(&mut self, _surface: PopupSurface) {
        // Its toplevel may be gone already, so every window recomposes.
        for w in &mut self.windows {
            w.dirty = true;
        }
    }

    fn title_changed(&mut self, surface: ToplevelSurface) {
        if let Some(i) = self.window_index(surface.wl_surface()) {
            self.update_title(i);
        }
    }

    fn app_id_changed(&mut self, surface: ToplevelSurface) {
        if let Some(i) = self.window_index(surface.wl_surface()) {
            self.update_title(i);
        }
    }
}

impl XdgDecorationHandler for State {
    fn new_decoration(&mut self, toplevel: ToplevelSurface) {
        toplevel.with_pending_state(|state| state.decoration_mode = Some(DecorationMode::ServerSide));
        if toplevel.is_initial_configure_sent() {
            toplevel.send_pending_configure();
        }
    }

    fn request_mode(&mut self, toplevel: ToplevelSurface, _mode: DecorationMode) {
        // Always server side, because the tile head is the title bar.
        self.new_decoration(toplevel);
    }

    fn unset_mode(&mut self, toplevel: ToplevelSurface) {
        self.new_decoration(toplevel);
    }
}

impl ShmHandler for State {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

impl OutputHandler for State {}

impl SeatHandler for State {
    type KeyboardFocus = Focus;
    type PointerFocus = Focus;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<State> {
        &mut self.seat_state
    }

    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&Focus>) {
        // The selections are offered to the client with keyboard focus.
        let client = focused.and_then(|f| f.wl_surface()).and_then(|s| self.dh.get_client(s.id()).ok());
        set_data_device_focus(&self.dh, seat, client.clone());
        set_primary_focus(&self.dh, seat, client);
    }

    fn cursor_image(&mut self, _seat: &Seat<Self>, image: CursorImageStatus) {
        if let CursorImageStatus::Surface(s) = &image {
            self.enter_output(s);
        }
        self.cursor.status = image;
        self.cursor.dirty = true;
    }
}

impl TabletSeatHandler for State {}

impl SelectionHandler for State {
    type SelectionUserData = Offer;

    fn new_selection(&mut self, target: SelectionTarget, source: Option<SelectionSource>, _seat: Seat<Self>) {
        // A Wayland client with keyboard focus set or cleared a selection.
        // smithay refuses it from clients without focus.
        let mimes = source.map(|s| s.mime_types());
        self.clipboard.owners[slot(target)] = if mimes.is_some() { Owner::Client } else { Owner::Nobody };
        self.announce_to_x(target, mimes.clone(), None);
        if let Some(mimes) = mimes {
            self.read_selection(target, &mimes);
        }
    }

    fn send_selection(&mut self, target: SelectionTarget, mime_type: String, fd: OwnedFd, _seat: Seat<Self>, offer: &Offer) {
        match offer {
            Offer::Text(text) => write_later(fd, text.clone()),
            Offer::X11(id) => self.read_from_x(*id, target, mime_type, fd),
        }
    }
}

impl DataDeviceHandler for State {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.data_device_state
    }
}

impl ClientDndGrabHandler for State {}
impl ServerDndGrabHandler for State {}

impl PrimarySelectionHandler for State {
    fn primary_selection_state(&self) -> &PrimarySelectionState {
        &self.primary_selection_state
    }
}

impl FractionalScaleHandler for State {
    fn new_fractional_scale(&mut self, surface: WlSurface) {
        let scale = self.scale;
        with_states(&surface, |states| with_fractional_scale(states, |fs| fs.set_preferred_scale(scale)));
    }
}

delegate_compositor!(State);
delegate_shm!(State);
delegate_xdg_shell!(State);
delegate_xdg_decoration!(State);
delegate_seat!(State);
delegate_data_device!(State);
delegate_primary_selection!(State);
delegate_output!(State);
delegate_viewporter!(State);
delegate_fractional_scale!(State);
delegate_cursor_shape!(State);
delegate_xwayland_shell!(State);
