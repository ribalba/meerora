//! Key names of `app_act key {keys}` and characters of `type {text}` (D30):
//! for terminals as the bytes an xterm sends, for real app windows as Linux
//! evdev codes of the US keyboard layout.
//!
//! `keys` holds one or more combinations separated by spaces or commas,
//! such as `ctrl+c`, `enter`, `up up enter` or `ctrl+shift+t`. Modifiers are
//! ctrl, alt, shift and super. Keys are enter, tab, esc, backspace, delete,
//! insert, home, end, pageup, pagedown, up, down, left, right, space, f1 to
//! f12 and single characters.

use winit::keyboard::{KeyCode, PhysicalKey};

/// A named key or a character.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Named {
    Enter,
    Tab,
    Escape,
    Backspace,
    Delete,
    Insert,
    Home,
    End,
    PageUp,
    PageDown,
    Up,
    Down,
    Left,
    Right,
    Space,
    F(u8),
    Char(char),
}

/// One key with its modifiers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Combo {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub meta: bool,
    pub key: Option<Named>,
}

/// Reads `keys`. The error names the part it could not read.
pub fn parse(keys: &str) -> Result<Vec<Combo>, String> {
    let parts: Vec<&str> = keys.split([' ', ',']).map(str::trim).filter(|p| !p.is_empty()).collect();
    if parts.is_empty() {
        return Err("keys is empty. Give keys such as \"enter\", \"ctrl+c\" or \"up up enter\".".into());
    }
    if parts.len() > 50 {
        return Err("keys may hold at most 50 keys at once.".into());
    }
    parts.into_iter().map(combo).collect()
}

fn combo(part: &str) -> Result<Combo, String> {
    let mut c = Combo::default();
    // A lone "+" is the plus key, "ctrl++" is Ctrl with plus.
    let (mods, key) = match part.strip_suffix("++") {
        Some(mods) => (mods, "+"),
        None => part.rsplit_once('+').filter(|(m, _)| !m.is_empty()).unwrap_or(("", part)),
    };
    for m in mods.split('+').filter(|m| !m.is_empty()) {
        match m.to_lowercase().as_str() {
            "ctrl" | "control" => c.ctrl = true,
            "alt" | "option" => c.alt = true,
            "shift" => c.shift = true,
            "super" | "meta" | "win" | "cmd" => c.meta = true,
            other => return Err(format!("{other:?} in {part:?} is not a modifier. Modifiers are ctrl, alt, shift and super.")),
        }
    }
    let lower = key.to_lowercase();
    let named = match lower.as_str() {
        "enter" | "return" => Named::Enter,
        "tab" => Named::Tab,
        "esc" | "escape" => Named::Escape,
        "backspace" => Named::Backspace,
        "delete" | "del" => Named::Delete,
        "insert" | "ins" => Named::Insert,
        "home" => Named::Home,
        "end" => Named::End,
        "pageup" | "pgup" => Named::PageUp,
        "pagedown" | "pgdn" => Named::PageDown,
        "up" => Named::Up,
        "down" => Named::Down,
        "left" => Named::Left,
        "right" => Named::Right,
        "space" => Named::Space,
        f if f.len() >= 2 && f.starts_with('f') && f[1..].parse::<u8>().is_ok_and(|n| (1..=12).contains(&n)) => {
            Named::F(f[1..].parse().unwrap_or(1))
        }
        _ => {
            let mut chars = key.chars();
            match (chars.next(), chars.next()) {
                (Some(ch), None) => Named::Char(ch),
                _ => {
                    return Err(format!(
                        "{key:?} is not a key. Use enter, tab, esc, backspace, delete, insert, home, end, pageup, pagedown, up, down, \
                         left, right, space, f1 to f12 or one character, with ctrl+, alt+, shift+ or super+ in front."
                    ));
                }
            }
        }
    };
    c.key = Some(named);
    Ok(c)
}

/// The bytes a terminal program gets for `combo`. `app_cursor` is the
/// cursor key mode of the terminal (DECCKM).
pub fn terminal_bytes(combo: &Combo, app_cursor: bool) -> Result<Vec<u8>, String> {
    use meerora_term::Key as T;
    let key = combo.key.ok_or("a key is missing")?;
    if combo.meta {
        return Err("A terminal has no super key.".into());
    }
    let mods = meerora_term::Mods { ctrl: combo.ctrl, alt: combo.alt, shift: combo.shift };
    let term_key = match key {
        Named::Enter => T::Enter,
        Named::Tab => T::Tab,
        Named::Escape => T::Escape,
        Named::Backspace => T::Backspace,
        Named::Delete => T::Delete,
        Named::Insert => T::Insert,
        Named::Home => T::Home,
        Named::End => T::End,
        Named::PageUp => T::PageUp,
        Named::PageDown => T::PageDown,
        Named::Up => T::Up,
        Named::Down => T::Down,
        Named::Left => T::Left,
        Named::Right => T::Right,
        Named::F(n) => T::F(n),
        Named::Space if !combo.ctrl && !combo.alt => return Ok(b" ".to_vec()),
        Named::Space => T::Char(' '),
        // A plain character is its text, shift makes letters capitals.
        Named::Char(ch) if !combo.ctrl && !combo.alt => {
            let ch = if combo.shift { ch.to_uppercase().next().unwrap_or(ch) } else { ch };
            return Ok(ch.to_string().into_bytes());
        }
        Named::Char(ch) => T::Char(ch.to_ascii_lowercase()),
    };
    meerora_term::encode_key(term_key, mods, app_cursor).ok_or_else(|| format!("The terminal has no sequence for {key:?}."))
}

/// The evdev code of a key of the US layout.
fn code(key: KeyCode) -> u32 {
    crate::keymap::evdev(PhysicalKey::Code(key)).unwrap_or(0)
}

/// The evdev code of the key that types `ch` on the US layout and whether
/// it needs Shift. None for characters that layout cannot type.
pub fn evdev_char(ch: char) -> Option<(u32, bool)> {
    use KeyCode as K;
    let letter = |c: char| -> Option<KeyCode> {
        Some(match c.to_ascii_lowercase() {
            'a' => K::KeyA,
            'b' => K::KeyB,
            'c' => K::KeyC,
            'd' => K::KeyD,
            'e' => K::KeyE,
            'f' => K::KeyF,
            'g' => K::KeyG,
            'h' => K::KeyH,
            'i' => K::KeyI,
            'j' => K::KeyJ,
            'k' => K::KeyK,
            'l' => K::KeyL,
            'm' => K::KeyM,
            'n' => K::KeyN,
            'o' => K::KeyO,
            'p' => K::KeyP,
            'q' => K::KeyQ,
            'r' => K::KeyR,
            's' => K::KeyS,
            't' => K::KeyT,
            'u' => K::KeyU,
            'v' => K::KeyV,
            'w' => K::KeyW,
            'x' => K::KeyX,
            'y' => K::KeyY,
            'z' => K::KeyZ,
            _ => return None,
        })
    };
    let (key, shift) = match ch {
        'a'..='z' => (letter(ch)?, false),
        'A'..='Z' => (letter(ch)?, true),
        '1' => (K::Digit1, false),
        '2' => (K::Digit2, false),
        '3' => (K::Digit3, false),
        '4' => (K::Digit4, false),
        '5' => (K::Digit5, false),
        '6' => (K::Digit6, false),
        '7' => (K::Digit7, false),
        '8' => (K::Digit8, false),
        '9' => (K::Digit9, false),
        '0' => (K::Digit0, false),
        '!' => (K::Digit1, true),
        '@' => (K::Digit2, true),
        '#' => (K::Digit3, true),
        '$' => (K::Digit4, true),
        '%' => (K::Digit5, true),
        '^' => (K::Digit6, true),
        '&' => (K::Digit7, true),
        '*' => (K::Digit8, true),
        '(' => (K::Digit9, true),
        ')' => (K::Digit0, true),
        '-' => (K::Minus, false),
        '_' => (K::Minus, true),
        '=' => (K::Equal, false),
        '+' => (K::Equal, true),
        '[' => (K::BracketLeft, false),
        '{' => (K::BracketLeft, true),
        ']' => (K::BracketRight, false),
        '}' => (K::BracketRight, true),
        '\\' => (K::Backslash, false),
        '|' => (K::Backslash, true),
        ';' => (K::Semicolon, false),
        ':' => (K::Semicolon, true),
        '\'' => (K::Quote, false),
        '"' => (K::Quote, true),
        '`' => (K::Backquote, false),
        '~' => (K::Backquote, true),
        ',' => (K::Comma, false),
        '<' => (K::Comma, true),
        '.' => (K::Period, false),
        '>' => (K::Period, true),
        '/' => (K::Slash, false),
        '?' => (K::Slash, true),
        ' ' => (K::Space, false),
        '\n' => (K::Enter, false),
        '\t' => (K::Tab, false),
        _ => return None,
    };
    let evdev = code(key);
    (evdev != 0).then_some((evdev, shift))
}

/// The evdev codes of `combo` in the order they go down: the modifiers,
/// then the key. They come up in reverse.
pub fn evdev_combo(combo: &Combo) -> Result<Vec<u32>, String> {
    use KeyCode as K;
    let key = combo.key.ok_or("a key is missing")?;
    let mut codes = Vec::new();
    for (on, key) in [(combo.ctrl, K::ControlLeft), (combo.alt, K::AltLeft), (combo.shift, K::ShiftLeft), (combo.meta, K::SuperLeft)] {
        if on {
            codes.push(code(key));
        }
    }
    let main = match key {
        Named::Enter => code(K::Enter),
        Named::Tab => code(K::Tab),
        Named::Escape => code(K::Escape),
        Named::Backspace => code(K::Backspace),
        Named::Delete => code(K::Delete),
        Named::Insert => code(K::Insert),
        Named::Home => code(K::Home),
        Named::End => code(K::End),
        Named::PageUp => code(K::PageUp),
        Named::PageDown => code(K::PageDown),
        Named::Up => code(K::ArrowUp),
        Named::Down => code(K::ArrowDown),
        Named::Left => code(K::ArrowLeft),
        Named::Right => code(K::ArrowRight),
        Named::Space => code(K::Space),
        Named::F(n) => {
            code([K::F1, K::F2, K::F3, K::F4, K::F5, K::F6, K::F7, K::F8, K::F9, K::F10, K::F11, K::F12][usize::from(n.clamp(1, 12) - 1)])
        }
        Named::Char(ch) => {
            let (code, shift) = evdev_char(ch).ok_or_else(|| format!("{ch:?} is not a key of the US keyboard layout."))?;
            if shift && !combo.shift {
                codes.push(self::code(K::ShiftLeft));
            }
            code
        }
    };
    codes.push(main);
    Ok(codes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(keys: &str, app_cursor: bool) -> Vec<u8> {
        parse(keys).unwrap().iter().flat_map(|c| terminal_bytes(c, app_cursor).unwrap()).collect()
    }

    #[test]
    fn key_names_become_terminal_bytes() {
        assert_eq!(bytes("ctrl+c", false), [0x03]);
        assert_eq!(bytes("enter", false), b"\r");
        assert_eq!(bytes("Return", false), b"\r");
        assert_eq!(bytes("up up enter", false), b"\x1b[A\x1b[A\r");
        assert_eq!(bytes("up", true), b"\x1bOA", "application cursor mode");
        assert_eq!(bytes("esc, tab", false), b"\x1b\t");
        assert_eq!(bytes("ctrl+d", false), [0x04]);
        assert_eq!(bytes("alt+b", false), b"\x1bb");
        assert_eq!(bytes("shift+tab", false), b"\x1b[Z");
        assert_eq!(bytes("q", false), b"q");
        assert_eq!(bytes("shift+q", false), b"Q");
        assert_eq!(bytes("space", false), b" ");
        assert_eq!(bytes("f1", false), b"\x1bOP");
        assert!(!bytes("ctrl++", false).is_empty());
        assert!(parse("").is_err());
        assert!(parse("hyper+x").unwrap_err().contains("not a modifier"));
        assert!(parse("ctrl+enterr").unwrap_err().contains("is not a key"));
        assert!(terminal_bytes(&parse("super+a").unwrap()[0], false).is_err());
    }

    #[test]
    fn characters_and_combinations_become_evdev_codes() {
        // From linux/input-event-codes.h.
        assert_eq!(evdev_char('a'), Some((30, false)));
        assert_eq!(evdev_char('A'), Some((30, true)));
        assert_eq!(evdev_char('1'), Some((2, false)));
        assert_eq!(evdev_char('!'), Some((2, true)));
        assert_eq!(evdev_char(' '), Some((57, false)));
        assert_eq!(evdev_char('\n'), Some((28, false)));
        assert_eq!(evdev_char(':'), Some((39, true)));
        assert_eq!(evdev_char('é'), None);
        assert_eq!(evdev_char('ß'), None);
        let combo = |k: &str| evdev_combo(&parse(k).unwrap()[0]).unwrap();
        assert_eq!(combo("ctrl+s"), [29, 31]);
        assert_eq!(combo("ctrl+shift+t"), [29, 42, 20]);
        assert_eq!(combo("enter"), [28]);
        assert_eq!(combo("?"), [42, 53], "shift comes with the character");
        assert_eq!(combo("super+left"), [125, 105]);
        assert_eq!(combo("f12"), [88]);
    }
}
