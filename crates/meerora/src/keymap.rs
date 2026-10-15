//! Physical keys for windows of the nested compositor (D15 phase 2). A
//! focused window gets the key a finger pressed, as a Linux evdev code,
//! and interprets it with the compositor's keymap. On Wayland and X11,
//! winit's scancode is that evdev code, so the inverse of winit's own
//! table (`PhysicalKeyExtScancode::to_scancode`) is the mapping.

use winit::keyboard::PhysicalKey;
use winit::platform::scancode::PhysicalKeyExtScancode as _;

/// The evdev code of a physical key, or `None` for keys winit cannot name.
pub fn evdev(key: PhysicalKey) -> Option<u32> {
    key.to_scancode()
}

#[cfg(test)]
mod tests {
    use super::*;
    use winit::keyboard::KeyCode;

    fn evdev_of(code: KeyCode) -> Option<u32> {
        evdev(PhysicalKey::Code(code))
    }

    #[test]
    fn known_keys_have_their_evdev_codes() {
        // From linux/input-event-codes.h.
        let known = [
            (KeyCode::Escape, 1),
            (KeyCode::Digit1, 2),
            (KeyCode::Digit0, 11),
            (KeyCode::Minus, 12),
            (KeyCode::Backspace, 14),
            (KeyCode::Tab, 15),
            (KeyCode::KeyQ, 16),
            (KeyCode::KeyP, 25),
            (KeyCode::Enter, 28),
            (KeyCode::ControlLeft, 29),
            (KeyCode::KeyA, 30),
            (KeyCode::KeyL, 38),
            (KeyCode::ShiftLeft, 42),
            (KeyCode::KeyZ, 44),
            (KeyCode::KeyV, 47),
            (KeyCode::KeyM, 50),
            (KeyCode::Slash, 53),
            (KeyCode::ShiftRight, 54),
            (KeyCode::AltLeft, 56),
            (KeyCode::Space, 57),
            (KeyCode::CapsLock, 58),
            (KeyCode::F1, 59),
            (KeyCode::F10, 68),
            (KeyCode::F11, 87),
            (KeyCode::F12, 88),
            (KeyCode::ControlRight, 97),
            (KeyCode::AltRight, 100),
            (KeyCode::Home, 102),
            (KeyCode::ArrowUp, 103),
            (KeyCode::PageUp, 104),
            (KeyCode::ArrowLeft, 105),
            (KeyCode::ArrowRight, 106),
            (KeyCode::End, 107),
            (KeyCode::ArrowDown, 108),
            (KeyCode::PageDown, 109),
            (KeyCode::Insert, 110),
            (KeyCode::Delete, 111),
            (KeyCode::SuperLeft, 125),
            (KeyCode::SuperRight, 126),
            (KeyCode::ContextMenu, 127),
            (KeyCode::IntlBackslash, 86),
            (KeyCode::Backquote, 41),
        ];
        for (key, code) in known {
            assert_eq!(evdev_of(key), Some(code), "{key:?}");
        }
    }

    #[test]
    fn the_mapping_inverts_winits_table() {
        let mut named = 0;
        for code in 1..=255u32 {
            if let PhysicalKey::Code(key) = PhysicalKey::from_scancode(code) {
                named += 1;
                let back = evdev_of(key).unwrap_or_else(|| panic!("{key:?} from {code} has no evdev code"));
                // A few evdev codes share one KeyCode, they come back as the first of them.
                assert_eq!(PhysicalKey::from_scancode(back), PhysicalKey::Code(key), "{code} -> {key:?} -> {back}");
            }
        }
        assert!(named > 100, "only {named} named keys");
    }
}
