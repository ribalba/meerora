//! The seat's keyboard and pointer point at a Wayland surface or an X11
//! window. An X11 window needs its own target, because the window
//! manager has to give it the X input focus before Xwayland sends it keys.

use std::borrow::Cow;

use smithay::backend::input::KeyState;
use smithay::desktop::PopupKind;
use smithay::input::Seat;
use smithay::input::keyboard::{KeyboardTarget, KeysymHandle, ModifiersState};
use smithay::input::pointer::{
    AxisFrame, ButtonEvent, GestureHoldBeginEvent, GestureHoldEndEvent, GesturePinchBeginEvent, GesturePinchEndEvent,
    GesturePinchUpdateEvent, GestureSwipeBeginEvent, GestureSwipeEndEvent, GestureSwipeUpdateEvent, MotionEvent, PointerTarget,
    RelativeMotionEvent,
};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{IsAlive, Serial};
use smithay::wayland::seat::WaylandFocus;
use smithay::xwayland::X11Surface;

use crate::state::State;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Focus {
    Surface(WlSurface),
    X11(X11Surface),
}

/// Runs the same call on whichever target it is.
macro_rules! on_target {
    ($focus:expr, $t:ident => $call:expr) => {
        match $focus {
            Focus::Surface($t) => $call,
            Focus::X11($t) => $call,
        }
    };
}

impl IsAlive for Focus {
    fn alive(&self) -> bool {
        on_target!(self, t => t.alive())
    }
}

impl WaylandFocus for Focus {
    fn wl_surface(&self) -> Option<Cow<'_, WlSurface>> {
        match self {
            Focus::Surface(s) => Some(Cow::Borrowed(s)),
            Focus::X11(x) => x.wl_surface().map(Cow::Owned),
        }
    }
}

impl From<PopupKind> for Focus {
    fn from(popup: PopupKind) -> Self {
        Focus::Surface(popup.wl_surface().clone())
    }
}

impl KeyboardTarget<State> for Focus {
    fn enter(&self, seat: &Seat<State>, data: &mut State, keys: Vec<KeysymHandle<'_>>, serial: Serial) {
        on_target!(self, t => KeyboardTarget::enter(t, seat, data, keys, serial))
    }

    fn leave(&self, seat: &Seat<State>, data: &mut State, serial: Serial) {
        on_target!(self, t => KeyboardTarget::leave(t, seat, data, serial))
    }

    fn key(&self, seat: &Seat<State>, data: &mut State, key: KeysymHandle<'_>, state: KeyState, serial: Serial, time: u32) {
        on_target!(self, t => KeyboardTarget::key(t, seat, data, key, state, serial, time))
    }

    fn modifiers(&self, seat: &Seat<State>, data: &mut State, modifiers: ModifiersState, serial: Serial) {
        on_target!(self, t => KeyboardTarget::modifiers(t, seat, data, modifiers, serial))
    }
}

impl PointerTarget<State> for Focus {
    fn enter(&self, seat: &Seat<State>, data: &mut State, event: &MotionEvent) {
        on_target!(self, t => PointerTarget::enter(t, seat, data, event))
    }

    fn motion(&self, seat: &Seat<State>, data: &mut State, event: &MotionEvent) {
        on_target!(self, t => PointerTarget::motion(t, seat, data, event))
    }

    fn relative_motion(&self, seat: &Seat<State>, data: &mut State, event: &RelativeMotionEvent) {
        on_target!(self, t => PointerTarget::relative_motion(t, seat, data, event))
    }

    fn button(&self, seat: &Seat<State>, data: &mut State, event: &ButtonEvent) {
        on_target!(self, t => PointerTarget::button(t, seat, data, event))
    }

    fn axis(&self, seat: &Seat<State>, data: &mut State, frame: AxisFrame) {
        on_target!(self, t => PointerTarget::axis(t, seat, data, frame))
    }

    fn frame(&self, seat: &Seat<State>, data: &mut State) {
        on_target!(self, t => PointerTarget::frame(t, seat, data))
    }

    fn gesture_swipe_begin(&self, seat: &Seat<State>, data: &mut State, event: &GestureSwipeBeginEvent) {
        on_target!(self, t => PointerTarget::gesture_swipe_begin(t, seat, data, event))
    }

    fn gesture_swipe_update(&self, seat: &Seat<State>, data: &mut State, event: &GestureSwipeUpdateEvent) {
        on_target!(self, t => PointerTarget::gesture_swipe_update(t, seat, data, event))
    }

    fn gesture_swipe_end(&self, seat: &Seat<State>, data: &mut State, event: &GestureSwipeEndEvent) {
        on_target!(self, t => PointerTarget::gesture_swipe_end(t, seat, data, event))
    }

    fn gesture_pinch_begin(&self, seat: &Seat<State>, data: &mut State, event: &GesturePinchBeginEvent) {
        on_target!(self, t => PointerTarget::gesture_pinch_begin(t, seat, data, event))
    }

    fn gesture_pinch_update(&self, seat: &Seat<State>, data: &mut State, event: &GesturePinchUpdateEvent) {
        on_target!(self, t => PointerTarget::gesture_pinch_update(t, seat, data, event))
    }

    fn gesture_pinch_end(&self, seat: &Seat<State>, data: &mut State, event: &GesturePinchEndEvent) {
        on_target!(self, t => PointerTarget::gesture_pinch_end(t, seat, data, event))
    }

    fn gesture_hold_begin(&self, seat: &Seat<State>, data: &mut State, event: &GestureHoldBeginEvent) {
        on_target!(self, t => PointerTarget::gesture_hold_begin(t, seat, data, event))
    }

    fn gesture_hold_end(&self, seat: &Seat<State>, data: &mut State, event: &GestureHoldEndEvent) {
        on_target!(self, t => PointerTarget::gesture_hold_end(t, seat, data, event))
    }

    fn leave(&self, seat: &Seat<State>, data: &mut State, serial: Serial, time: u32) {
        on_target!(self, t => PointerTarget::leave(t, seat, data, serial, time))
    }
}
