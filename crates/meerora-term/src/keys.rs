//! xterm key encoding.

use crate::{Key, Mods};

const ESC: u8 = 0x1b;

pub(crate) fn encode(key: Key, mods: Mods, app_cursor: bool) -> Option<Vec<u8>> {
    // xterm modifier parameter: 1 plus Shift 1, Alt 2, Ctrl 4.
    let param = 1 + u8::from(mods.shift) + 2 * u8::from(mods.alt) + 4 * u8::from(mods.ctrl);
    let bytes = match key {
        Key::Enter => alt_prefixed(mods, b"\r"),
        Key::Backspace if mods.ctrl => alt_prefixed(mods, b"\x08"),
        Key::Backspace => alt_prefixed(mods, b"\x7f"),
        Key::Tab if mods.shift => b"\x1b[Z".to_vec(),
        Key::Tab => alt_prefixed(mods, b"\t"),
        Key::Escape => alt_prefixed(mods, b"\x1b"),
        Key::Up => cursor(b'A', param, app_cursor),
        Key::Down => cursor(b'B', param, app_cursor),
        Key::Right => cursor(b'C', param, app_cursor),
        Key::Left => cursor(b'D', param, app_cursor),
        Key::Home => cursor(b'H', param, app_cursor),
        Key::End => cursor(b'F', param, app_cursor),
        Key::Insert => tilde(2, param),
        Key::Delete => tilde(3, param),
        Key::PageUp => tilde(5, param),
        Key::PageDown => tilde(6, param),
        Key::F(n @ 1..=4) => ss3(b"PQRS"[usize::from(n - 1)], param),
        Key::F(n @ 5..=12) => tilde([15, 17, 18, 19, 20, 21, 23, 24][usize::from(n - 5)], param),
        Key::F(_) => return None,
        Key::Char(c) => character(c, mods),
    };
    Some(bytes)
}

fn alt_prefixed(mods: Mods, bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() + 1);
    if mods.alt {
        out.push(ESC);
    }
    out.extend_from_slice(bytes);
    out
}

/// Cursor keys, Home and End: `CSI 1;m X` with modifiers, otherwise SS3 in
/// application cursor mode and CSI in normal mode.
fn cursor(final_byte: u8, param: u8, app_cursor: bool) -> Vec<u8> {
    if param > 1 {
        format!("\x1b[1;{param}{}", char::from(final_byte)).into_bytes()
    } else if app_cursor {
        vec![ESC, b'O', final_byte]
    } else {
        vec![ESC, b'[', final_byte]
    }
}

/// F1 to F4: SS3 without modifiers, `CSI 1;m X` with them.
fn ss3(final_byte: u8, param: u8) -> Vec<u8> {
    if param > 1 { format!("\x1b[1;{param}{}", char::from(final_byte)).into_bytes() } else { vec![ESC, b'O', final_byte] }
}

/// Editing and function keys of the form `CSI n ~` or `CSI n;m ~`.
fn tilde(code: u8, param: u8) -> Vec<u8> {
    if param > 1 { format!("\x1b[{code};{param}~").into_bytes() } else { format!("\x1b[{code}~").into_bytes() }
}

fn character(c: char, mods: Mods) -> Vec<u8> {
    let mut out = Vec::with_capacity(5);
    if mods.alt {
        out.push(ESC);
    }
    match control_code(c).filter(|_| mods.ctrl) {
        Some(code) => out.push(code),
        None => out.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes()),
    }
    out
}

/// The C0 code xterm sends for Ctrl plus `c`, None when Ctrl leaves `c` as it is.
fn control_code(c: char) -> Option<u8> {
    Some(match c {
        'a'..='z' | 'A'..='Z' => c as u8 & 0x1f,
        '@' | ' ' | '2' => 0x00,
        '[' | '3' => 0x1b,
        '\\' | '4' => 0x1c,
        ']' | '5' => 0x1d,
        '^' | '6' | '~' => 0x1e,
        '_' | '7' | '/' | '-' => 0x1f,
        '8' | '?' => 0x7f,
        _ => return None,
    })
}
