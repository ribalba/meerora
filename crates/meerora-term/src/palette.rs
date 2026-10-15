//! The terminal palette and the resolution of grid cells to sRGB.

use alacritty_terminal::term::cell::{Cell as GridCell, Flags};
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::vte::ansi::{Color, Rgb};

use crate::Cell;

/// Default foreground.
pub const FOREGROUND: [u8; 3] = [0xE8, 0xEA, 0xF0];

/// Default background.
pub const BACKGROUND: [u8; 3] = [0x0E, 0x11, 0x17];

/// ANSI colours 0 to 7 followed by their bright variants 8 to 15.
pub const ANSI: [[u8; 3]; 16] = [
    [0x1B, 0x1F, 0x28],
    [0xE0, 0x67, 0x5A],
    [0x7F, 0xD4, 0xA0],
    [0xF2, 0xB4, 0x5A],
    [0x8C, 0xD3, 0xFF],
    [0xB7, 0xA6, 0xFF],
    [0x7F, 0xE3, 0xD0],
    [0xC9, 0xCE, 0xD9],
    [0x4A, 0x51, 0x60],
    [0xF0, 0x8A, 0x7E],
    [0xA3, 0xE6, 0xBD],
    [0xFF, 0xD0, 0x8A],
    [0xBD, 0xE6, 0xFF],
    [0xD3, 0xC7, 0xFF],
    [0xA8, 0xF0, 0xE3],
    [0xF4, 0xF5, 0xF8],
];

/// Channel levels of the xterm 6x6x6 colour cube.
const CUBE_LEVELS: [u8; 6] = [0x00, 0x5F, 0x87, 0xAF, 0xD7, 0xFF];

/// Colour `index` of the 256-colour palette: the 16 ANSI colours, the 6x6x6
/// cube and the 24-step grey ramp.
pub fn indexed(index: u8) -> [u8; 3] {
    match index {
        0..=15 => ANSI[usize::from(index)],
        16..=231 => {
            let i = usize::from(index - 16);
            [CUBE_LEVELS[i / 36], CUBE_LEVELS[(i / 6) % 6], CUBE_LEVELS[i % 6]]
        }
        232..=255 => {
            let level = 8 + 10 * (index - 232);
            [level; 3]
        }
    }
}

/// Default colour of an alacritty colour slot (0 to 268, see
/// `alacritty_terminal::term::color`).
pub(crate) fn slot(index: usize) -> [u8; 3] {
    match index {
        0..=255 => indexed(index as u8),
        257 => BACKGROUND,
        259..=266 => dim(ANSI[index - 259]),
        268 => dim(FOREGROUND),
        _ => FOREGROUND,
    }
}

/// Colour of a slot, preferring a value the program set through OSC 4, 10 or 11.
fn lookup(index: usize, overrides: &Colors) -> [u8; 3] {
    match overrides[index] {
        Some(rgb) => rgb_array(rgb),
        None => slot(index),
    }
}

fn rgb_array(rgb: Rgb) -> [u8; 3] {
    [rgb.r, rgb.g, rgb.b]
}

/// Dims a colour to two thirds of its intensity.
pub(crate) fn dim(rgb: [u8; 3]) -> [u8; 3] {
    rgb.map(|c| ((u16::from(c) * 2 + 1) / 3) as u8)
}

/// Resolves a grid cell to the public [`Cell`], applying bold-as-bright for
/// the 8 base colours, dim, inverse and hidden.
pub(crate) fn resolve(cell: &GridCell, overrides: &Colors) -> Cell {
    let flags = cell.flags;
    let bold = flags.contains(Flags::BOLD);
    let brighten = |index: usize| if bold && index < 8 { index + 8 } else { index };

    let mut fg = match cell.fg {
        Color::Spec(rgb) => rgb_array(rgb),
        Color::Named(name) => lookup(brighten(name as usize), overrides),
        Color::Indexed(index) => lookup(brighten(usize::from(index)), overrides),
    };
    if flags.contains(Flags::DIM) {
        fg = dim(fg);
    }
    let mut bg = match cell.bg {
        Color::Spec(rgb) => rgb_array(rgb),
        Color::Named(name) => lookup(name as usize, overrides),
        Color::Indexed(index) => lookup(usize::from(index), overrides),
    };
    if flags.contains(Flags::INVERSE) {
        std::mem::swap(&mut fg, &mut bg);
    }
    if flags.contains(Flags::HIDDEN) {
        fg = bg;
    }

    let spacer = flags.contains(Flags::WIDE_CHAR_SPACER);
    let blank = spacer || flags.contains(Flags::LEADING_WIDE_CHAR_SPACER) || cell.c == '\t';
    Cell {
        ch: if blank { ' ' } else { cell.c },
        fg,
        bg,
        bold,
        italic: flags.contains(Flags::ITALIC),
        underline: flags.intersects(Flags::ALL_UNDERLINES),
        spacer,
    }
}
