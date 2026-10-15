//! Xwayland per desktop realm, and the X11 window manager that turns X11
//! windows into windows of their desktop (smithay's `X11Wm`).
//!
//! Each realm of a desktop gets its own X display, started with the first
//! program launched in that realm. meerora cannot tell an X11-only app
//! before it runs, and the X socket accepts connections before Xwayland is
//! ready, so a launch never waits for it. A desktop that only ever runs
//! Wayland apps through `launch` still gets an X server, which costs a few
//! megabytes, and [`crate::LaunchOptions::x11`] can leave it out.
//!
//! Apps the user starts and apps an agent starts never share an X server.
//! X11 has no isolation between the clients of one server. Any client can read
//! the keys of another or send it input. The agent realm's server also runs
//! without MIT-SHM, because sandboxed clients live in their own IPC
//! namespace and a shared memory id from there would name a segment of the
//! host in the server.
//!
//! X11 toplevels all sit at the origin of the X screen, and a window under
//! the pointer is raised, since Xwayland picks the window for a pointer
//! event by X stacking order. Override-redirect windows (menus, tooltips)
//! are drawn in the tile of the window they belong to.

use std::io;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{Client, DisplayHandle};
use smithay::utils::{Logical, Rectangle, Size};
use smithay::wayland::selection::SelectionTarget;
use smithay::wayland::selection::data_device::{clear_data_device_selection, set_data_device_selection};
use smithay::wayland::selection::primary_selection::{clear_primary_selection, set_primary_selection};
use smithay::wayland::xwayland_shell::{XWaylandShellHandler, XWaylandShellState};
use smithay::xwayland::xwm::{Reorder, ResizeEdge, WmWindowProperty, X11Window, XwmId};
use smithay::xwayland::{X11Surface, X11Wm, XWayland, XwmHandler};

use crate::state::{Offer, Owner, State, Win, XPopup, slot};
use crate::{Event, Realm};

/// The size an X11 window gets before its tile asks for one.
const FIRST_SIZE: (i32, i32) = (800, 600);

/// Which desktop and realm an Xwayland serves, kept in its client data.
#[derive(Clone, Debug)]
pub(crate) struct XOwner {
    pub agent: String,
    pub realm: Realm,
}

/// A running or starting Xwayland.
pub(crate) struct XServer {
    pub display: u32,
    pub xwayland: XWayland,
    pub client: Client,
    /// The window manager, once Xwayland is ready.
    pub wm: Option<X11Wm>,
    /// The X11 window raised last.
    pub raised: Option<u32>,
}

impl XServer {
    /// Starts Xwayland for `owner`. It renders without glamor, since the
    /// compositor takes shared memory buffers only. `wrapper` is a directory
    /// for a script that starts it without MIT-SHM, for the agent realm.
    pub fn spawn(dh: &DisplayHandle, owner: XOwner, wrapper: Option<&Path>) -> io::Result<XServer> {
        let mut env: Vec<(String, String)> = vec![("XWAYLAND_NO_GLAMOR".into(), "1".into())];
        if let Some(dir) = wrapper {
            write_wrapper(dir)?;
            let path = std::env::var("PATH").unwrap_or_default();
            env.push(("PATH".into(), format!("{}:{path}", dir.display())));
        }
        // No abstract socket. It lives in the network namespace, which a
        // sandbox with network shares, so it would reach past the mounts.
        let (xwayland, client) = XWayland::spawn(dh, None, env, false, Stdio::null(), Stdio::null(), |data| {
            data.insert_if_missing(|| owner);
        })?;
        let display = xwayland.display_number();
        Ok(XServer { display, xwayland, client, wm: None, raised: None })
    }

    /// The path of the X socket, for the sandbox.
    pub fn socket(&self) -> PathBuf {
        PathBuf::from(format!("/tmp/.X11-unix/X{}", self.display))
    }

    /// The connection for the window manager, once Xwayland is ready. `Err`
    /// when it died while starting.
    pub fn ready(&mut self) -> io::Result<Option<UnixStream>> {
        if self.wm.is_some() {
            return Ok(None);
        }
        self.xwayland.take_socket()
    }
}

/// A script named `Xwayland` in `dir` that starts the real one without
/// MIT-SHM. smithay starts `Xwayland` from PATH, so the directory goes first.
fn write_wrapper(dir: &Path) -> io::Result<()> {
    let real = std::env::var_os("PATH")
        .and_then(|path| std::env::split_paths(&path).map(|d| d.join("Xwayland")).find(|p| p.is_file() && !p.starts_with(dir)))
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "Xwayland is not installed"))?;
    std::fs::create_dir_all(dir)?;
    let script = dir.join("Xwayland");
    let quoted = real.display().to_string().replace('\'', r"'\''");
    std::fs::write(&script, format!("#!/bin/sh\nexec '{quoted}' -extension MIT-SHM \"$@\"\n"))?;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700))
}

impl State {
    /// The window an override-redirect window belongs to. That is the one
    /// it is transient for, else the X11 window of the same server with the
    /// keyboard or the pointer, else that server's newest window.
    fn popup_parent(&self, xwm: XwmId, surface: &X11Surface) -> Option<u32> {
        let of_server = |w: &&Win| w.xwm() == Some(xwm) && w.mapped;
        if let Some(parent) = surface.is_transient_for() {
            if let Some(w) = self.windows.iter().filter(of_server).find(|w| w.x11().is_some_and(|x| x.window_id() == parent)) {
                return Some(w.id);
            }
            if let Some(p) = self.x_popups.iter().find(|p| p.surface.window_id() == parent) {
                return Some(p.parent);
            }
        }
        if let Some(id) = self.focused.filter(|id| self.win(*id).is_some_and(|w| of_server(&w))) {
            return Some(id);
        }
        let raised = self
            .desktops
            .values()
            .flat_map(|d| d.places.iter())
            .filter_map(|p| p.x.as_ref())
            .find_map(|x| x.wm.as_ref().filter(|wm| wm.id() == xwm).and(x.raised));
        raised.or_else(|| self.windows.iter().rfind(of_server).map(|w| w.id))
    }

    /// Takes an X11 toplevel or menu out, with its menus.
    fn forget_x11(&mut self, surface: &X11Surface) {
        if let Some(i) = self.x_popups.iter().position(|p| &p.surface == surface) {
            let popup = self.x_popups.remove(i);
            self.touch(popup.parent);
            return;
        }
        let Some(i) = self.x_window_index(surface) else { return };
        let w = self.windows.remove(i);
        self.x_popups.retain(|p| p.parent != w.id);
        if self.focused == Some(w.id) {
            self.focused = None;
        }
        if w.mapped {
            self.push(Event::Closed(w.id));
        }
    }

    /// Raises an X11 window and its menus to the top of its X server.
    pub(crate) fn raise_x11(&mut self, id: u32) {
        let Some(w) = self.win(id) else { return };
        let (Some(surface), Some(xwm)) = (w.x11().cloned(), w.xwm()) else { return };
        let menus: Vec<X11Surface> = self.x_popups.iter().filter(|p| p.parent == id).map(|p| p.surface.clone()).collect();
        let Some(server) = self
            .desktops
            .values_mut()
            .flat_map(|d| d.places.iter_mut())
            .filter_map(|p| p.x.as_mut())
            .find(|x| x.wm.as_ref().is_some_and(|wm| wm.id() == xwm))
        else {
            return;
        };
        if server.raised == Some(id) {
            return;
        }
        server.raised = Some(id);
        if let Some(wm) = server.wm.as_mut() {
            let _ = wm.raise_window(&surface);
            for menu in &menus {
                let _ = wm.raise_window(menu);
            }
        }
    }

    /// An X client took or dropped a selection. It reaches the other
    /// clients and the host clipboard only while one of its windows has the
    /// keyboard, as a Wayland client's selection does.
    fn x_selection(&mut self, xwm: XwmId, target: SelectionTarget, mimes: Option<Vec<String>>) {
        let i = slot(target);
        match mimes {
            Some(mimes) => {
                if self.focused_xwm() != Some(xwm) {
                    tracing::debug!("an X client without the keyboard set the {target:?} selection, ignored");
                    return;
                }
                self.clipboard.owners[i] = Owner::X11(xwm);
                match target {
                    SelectionTarget::Clipboard => set_data_device_selection(&self.dh, &self.seat, mimes.clone(), Offer::X11(xwm)),
                    SelectionTarget::Primary => set_primary_selection(&self.dh, &self.seat, mimes.clone(), Offer::X11(xwm)),
                }
                self.announce_to_x(target, Some(mimes.clone()), Some(xwm));
                self.read_selection(target, &mimes);
            }
            None if self.clipboard.owners[i] == Owner::X11(xwm) => {
                self.clipboard.owners[i] = Owner::Nobody;
                match target {
                    SelectionTarget::Clipboard => clear_data_device_selection(&self.dh, &self.seat),
                    SelectionTarget::Primary => clear_primary_selection(&self.dh, &self.seat),
                }
                self.announce_to_x(target, None, Some(xwm));
            }
            None => {}
        }
    }

    /// The size an X11 window should have, its tile's or else what it asks for.
    fn x11_size(&self, surface: &X11Surface, asked: (Option<u32>, Option<u32>)) -> Size<i32, Logical> {
        if let Some(want) = self.x_window_index(surface).and_then(|i| self.windows[i].want) {
            return want;
        }
        let geo = surface.geometry();
        let w = asked.0.map(|w| w as i32).unwrap_or(geo.size.w);
        let h = asked.1.map(|h| h as i32).unwrap_or(geo.size.h);
        if w > 0 && h > 0 { Size::from((w, h)) } else { Size::from(FIRST_SIZE) }
    }
}

impl XwmHandler for State {
    fn xwm_state(&mut self, xwm: XwmId) -> &mut X11Wm {
        self.xwm(xwm).expect("X11 events come from a running window manager")
    }

    fn new_window(&mut self, _xwm: XwmId, _window: X11Surface) {}

    fn new_override_redirect_window(&mut self, _xwm: XwmId, _window: X11Surface) {}

    fn map_window_request(&mut self, xwm: XwmId, window: X11Surface) {
        let Some((agent, realm)) = self.xwm_owner(xwm) else { return };
        if let Err(e) = window.set_mapped(true) {
            tracing::warn!("cannot map an X11 window: {e}");
            return;
        }
        let size = self.x11_size(&window, (None, None));
        let _ = window.configure(Rectangle::new((0, 0).into(), size));
        if self.x_window_index(&window).is_some() {
            return;
        }
        self.next_id += 1;
        self.windows.push(Win::new(self.next_id, agent, realm, smithay::desktop::Window::new_x11_window(window)));
    }

    fn mapped_override_redirect_window(&mut self, xwm: XwmId, window: X11Surface) {
        if self.x_popups.iter().any(|p| p.surface == window) {
            return;
        }
        match self.popup_parent(xwm, &window) {
            Some(parent) => {
                self.x_popups.push(XPopup { surface: window, parent });
                self.touch(parent);
            }
            None => tracing::debug!("an X11 menu without a window to show it in"),
        }
    }

    fn unmapped_window(&mut self, _xwm: XwmId, window: X11Surface) {
        if !window.is_override_redirect() {
            let _ = window.set_mapped(false);
        }
        self.forget_x11(&window);
    }

    fn destroyed_window(&mut self, _xwm: XwmId, window: X11Surface) {
        self.forget_x11(&window);
    }

    fn configure_request(
        &mut self,
        _xwm: XwmId,
        window: X11Surface,
        _x: Option<i32>,
        _y: Option<i32>,
        w: Option<u32>,
        h: Option<u32>,
        _reorder: Option<Reorder>,
    ) {
        // Tiles place and size windows, so a mapped window keeps its tile's
        // size and every window stays at the origin.
        if window.is_override_redirect() {
            return;
        }
        let size = self.x11_size(&window, (w, h));
        let _ = window.configure(Rectangle::new((0, 0).into(), size));
    }

    fn configure_notify(&mut self, _xwm: XwmId, window: X11Surface, _geometry: Rectangle<i32, Logical>, _above: Option<X11Window>) {
        if let Some(parent) = self.x_popups.iter().find(|p| p.surface == window).map(|p| p.parent) {
            self.touch(parent);
        } else if let Some(i) = self.x_window_index(&window) {
            self.windows[i].dirty = true;
        }
    }

    fn property_notify(&mut self, _xwm: XwmId, window: X11Surface, property: WmWindowProperty) {
        if matches!(property, WmWindowProperty::Title | WmWindowProperty::Class)
            && let Some(i) = self.x_window_index(&window)
        {
            self.update_title(i);
        }
    }

    fn resize_request(&mut self, _xwm: XwmId, _window: X11Surface, _button: u32, _resize_edge: ResizeEdge) {}

    fn move_request(&mut self, _xwm: XwmId, _window: X11Surface, _button: u32) {}

    fn allow_selection_access(&mut self, xwm: XwmId, _selection: SelectionTarget) -> bool {
        // An X client reads a selection when it pastes, which needs the keyboard.
        self.focused_xwm() == Some(xwm)
    }

    fn send_selection(&mut self, xwm: XwmId, selection: SelectionTarget, mime_type: String, fd: std::os::fd::OwnedFd) {
        self.serve_to_x(xwm, selection, mime_type, fd);
    }

    fn new_selection(&mut self, xwm: XwmId, selection: SelectionTarget, mime_types: Vec<String>) {
        self.x_selection(xwm, selection, Some(mime_types));
    }

    fn cleared_selection(&mut self, xwm: XwmId, selection: SelectionTarget) {
        self.x_selection(xwm, selection, None);
    }

    fn disconnected(&mut self, xwm: XwmId) {
        tracing::info!("an X server of the compositor went away");
        let gone: Vec<X11Surface> = self.windows.iter().filter(|w| w.xwm() == Some(xwm)).filter_map(|w| w.x11().cloned()).collect();
        for surface in gone {
            self.forget_x11(&surface);
        }
        self.x_popups.retain(|p| p.surface.xwm_id() != Some(xwm));
        for owner in &mut self.clipboard.owners {
            if *owner == Owner::X11(xwm) {
                *owner = Owner::Nobody;
            }
        }
        self.retired.retain(|wm| wm.id() != xwm);
        // The next program of that realm starts a new one.
        for place in self.desktops.values_mut().flat_map(|d| d.places.iter_mut()) {
            if place.x.as_ref().is_some_and(|x| x.wm.as_ref().is_some_and(|wm| wm.id() == xwm)) {
                place.x = None;
            }
        }
    }
}

impl XWaylandShellHandler for State {
    fn xwayland_shell_state(&mut self) -> &mut XWaylandShellState {
        &mut self.xwayland_shell_state
    }

    fn surface_associated(&mut self, _xwm: XwmId, surface: WlSurface, window: X11Surface) {
        // Xwayland may have committed its first buffer before the window
        // manager learned which window the surface belongs to.
        self.enter_output(&surface);
        if let Some(i) = self.x_window_index(&window) {
            self.windows[i].dirty = true;
            self.check_mapped(i);
        } else if let Some(parent) = self.x_popups.iter().find(|p| p.surface == window).map(|p| p.parent) {
            self.touch(parent);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wrapper_turns_off_mit_shm() {
        if !crate::launch::installed("Xwayland") {
            eprintln!("skipped: Xwayland is not installed");
            return;
        }
        let dir = std::env::temp_dir().join(format!("meerora-xwrapper-{}", std::process::id()));
        write_wrapper(&dir).unwrap();
        let text = std::fs::read_to_string(dir.join("Xwayland")).unwrap();
        assert!(text.contains("-extension MIT-SHM \"$@\""), "{text}");
        assert!(!text.contains(&dir.display().to_string()), "it starts the real Xwayland, not itself");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
