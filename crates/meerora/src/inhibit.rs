//! Desktop shortcuts for meerora (D15 phase 1). While the meerora window
//! has the keyboard, Alt+Tab, Super and similar shortcuts should reach
//! meerora instead of the host desktop. The Wayland protocol
//! `zwp_keyboard_shortcuts_inhibit_manager_v1` asks the host compositor
//! for that on meerora's own surface. GNOME asks the user once whether to
//! allow it. Under X11, or when the host lacks the protocol, nothing happens.
//!
//! winit owns the Wayland connection. This module wraps the same
//! `wl_display` with wayland-client and talks on its own event queue.

use wayland_backend::client::{Backend, ObjectId};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_registry, wl_seat::WlSeat, wl_surface::WlSurface};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle};
use wayland_protocols::wp::keyboard_shortcuts_inhibit::zv1::client::zwp_keyboard_shortcuts_inhibit_manager_v1::ZwpKeyboardShortcutsInhibitManagerV1;
use wayland_protocols::wp::keyboard_shortcuts_inhibit::zv1::client::zwp_keyboard_shortcuts_inhibitor_v1::{
    self, ZwpKeyboardShortcutsInhibitorV1,
};
use winit::raw_window_handle::{HasDisplayHandle as _, HasWindowHandle as _, RawDisplayHandle, RawWindowHandle};

/// Whether the host compositor currently lets meerora have its shortcuts.
#[derive(Default)]
struct Status {
    active: bool,
}

pub struct Inhibitor {
    conn: Connection,
    queue: EventQueue<Status>,
    status: Status,
    manager: ZwpKeyboardShortcutsInhibitManagerV1,
    seat: WlSeat,
    surface: WlSurface,
    inhibitor: Option<ZwpKeyboardShortcutsInhibitorV1>,
}

impl Inhibitor {
    /// Binds the protocol on the window's Wayland connection. Errors say why
    /// there is nothing to do, for example under X11.
    pub fn new(window: &winit::window::Window) -> Result<Inhibitor, String> {
        let display = window.display_handle().map_err(|e| e.to_string())?.as_raw();
        let RawDisplayHandle::Wayland(display) = display else { return Err("not a Wayland session".into()) };
        let RawWindowHandle::Wayland(handle) = window.window_handle().map_err(|e| e.to_string())?.as_raw() else {
            return Err("not a Wayland window".into());
        };
        // SAFETY: winit keeps the display alive as long as the window, which outlives this struct in app.rs.
        let backend = unsafe { Backend::from_foreign_display(display.display.as_ptr().cast()) };
        let conn = Connection::from_backend(backend);
        let (globals, queue) = registry_queue_init::<Status>(&conn).map_err(|e| e.to_string())?;
        let qh = queue.handle();
        let manager = globals
            .bind::<ZwpKeyboardShortcutsInhibitManagerV1, _, _>(&qh, 1..=1, ())
            .map_err(|_| "the desktop does not offer keyboard-shortcuts-inhibit".to_string())?;
        let seat = globals.bind::<WlSeat, _, _>(&qh, 1..=1, ()).map_err(|e| e.to_string())?;
        // SAFETY: the pointer is winit's live wl_surface proxy of this window.
        let id = unsafe { ObjectId::from_ptr(WlSurface::interface(), handle.surface.as_ptr().cast()) }.map_err(|e| e.to_string())?;
        let surface = WlSurface::from_id(&conn, id).map_err(|e| e.to_string())?;
        Ok(Inhibitor { conn, queue, status: Status::default(), manager, seat, surface, inhibitor: None })
    }

    /// Called when the meerora window gets the keyboard. The inhibitor is
    /// made once and kept. The protocol applies it only while the surface
    /// has focus, and making it again would ask the user again on GNOME.
    pub fn focused(&mut self) {
        if self.inhibitor.is_none() {
            let qh = self.queue.handle();
            self.inhibitor = Some(self.manager.inhibit_shortcuts(&self.surface, &self.seat, &qh, ()));
            let _ = self.conn.flush();
            tracing::info!("asked the desktop to pass its shortcuts to meerora");
        }
    }

    /// Handles the host's answers, once per frame. winit reads the socket.
    pub fn dispatch(&mut self) {
        let was = self.status.active;
        if let Err(e) = self.queue.dispatch_pending(&mut self.status) {
            tracing::debug!("keyboard-shortcuts-inhibit queue: {e}");
        }
        if was != self.status.active {
            tracing::info!("desktop shortcuts go to {}", if self.status.active { "meerora" } else { "the desktop" });
        }
    }
}

impl Drop for Inhibitor {
    fn drop(&mut self) {
        if let Some(i) = self.inhibitor.take() {
            i.destroy();
        }
        self.manager.destroy();
        let _ = self.conn.flush();
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for Status {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlSeat, ()> for Status {
    fn event(_: &mut Self, _: &WlSeat, _: <WlSeat as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<ZwpKeyboardShortcutsInhibitManagerV1, ()> for Status {
    fn event(
        _: &mut Self,
        _: &ZwpKeyboardShortcutsInhibitManagerV1,
        _: <ZwpKeyboardShortcutsInhibitManagerV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpKeyboardShortcutsInhibitorV1, ()> for Status {
    fn event(
        status: &mut Self,
        _: &ZwpKeyboardShortcutsInhibitorV1,
        event: zwp_keyboard_shortcuts_inhibitor_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwp_keyboard_shortcuts_inhibitor_v1::Event::Active => status.active = true,
            zwp_keyboard_shortcuts_inhibitor_v1::Event::Inactive => status.active = false,
            _ => {}
        }
    }
}
