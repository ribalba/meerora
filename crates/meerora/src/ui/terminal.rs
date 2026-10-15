//! Draws a [`meerora_term::Terminal`] inside a tile and forwards keyboard,
//! paste and scroll input to its PTY while the tile has focus.

use egui::{Align2, Color32, CornerRadius, Event, EventFilter, Id, Key as EKey, Rect, Sense, Stroke, StrokeKind, Ui, pos2, vec2};
use meerora_term::{Key, Mods, TermSize, Terminal};

use crate::theme::{self, c};

const FONT_SIZE: f32 = 13.5;

/// Returns true while the terminal has keyboard focus.
pub fn show(ui: &mut Ui, rect: Rect, id: Id, term: &Terminal) -> bool {
    let font = theme::mono(FONT_SIZE);
    let (cell_w, cell_h) = ui.ctx().fonts_mut(|f| (f.glyph_width(&font, 'M'), f.row_height(&font).ceil()));
    let inner = rect.shrink2(vec2(12.0, 10.0));
    let size =
        TermSize { cols: ((inner.width() / cell_w).floor() as u16).max(10), rows: ((inner.height() / cell_h).floor() as u16).max(3) };
    let snap = term.snapshot();
    if snap.size != size {
        term.resize(size);
    }

    // Tab, Shift+Tab and the arrows belong to the shell while the terminal
    // has the keyboard. egui reads them as focus moves when the frame
    // begins, with the event filter of the frame before, and gives the focus
    // away when this widget registers. Right after the click that focused
    // the terminal no filter was set yet, so Tab went to the next widget and
    // never reached the PTY. The move is cancelled before registering.
    let filter = EventFilter { tab: true, horizontal_arrows: true, vertical_arrows: true, escape: true };
    if ui.memory(|m| m.has_focus(id)) {
        ui.memory_mut(|m| {
            m.set_focus_lock_filter(id, filter);
            m.move_focus(egui::FocusDirection::None);
        });
    }
    let resp = ui.interact(rect, id, Sense::click());
    // The press already focuses, so the frame of the release has the filter.
    if (resp.is_pointer_button_down_on() && !resp.has_focus()) || resp.clicked() {
        resp.request_focus();
    }
    let focused = resp.has_focus();
    if focused {
        ui.memory_mut(|m| m.set_focus_lock_filter(id, filter));
        forward_input(ui, term, snap.app_cursor);
    }
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::Text);
        let scroll = ui.input(|i| i.smooth_scroll_delta.y);
        if scroll.abs() > 0.5 {
            term.scroll((scroll / cell_h).round() as i32);
        }
    }

    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, CornerRadius::ZERO, c(theme::TERMINAL_BG));
    let default_bg = theme::TERMINAL_BG;
    let cols = snap.size.cols as usize;
    for row in 0..snap.size.rows as usize {
        let y = inner.min.y + row as f32 * cell_h;
        let cells = &snap.cells[row * cols..(row + 1) * cols];
        // Backgrounds first, merged into runs.
        let mut col = 0;
        while col < cols {
            let bg = cells[col].bg;
            let start = col;
            while col < cols && cells[col].bg == bg {
                col += 1;
            }
            if bg != default_bg {
                let r = Rect::from_min_size(pos2(inner.min.x + start as f32 * cell_w, y), vec2((col - start) as f32 * cell_w, cell_h));
                painter.rect_filled(r, CornerRadius::ZERO, c(bg));
            }
        }
        // Text in runs of equal colour and weight. Every run starts at its own column, so wide glyphs stay aligned.
        let mut col = 0;
        while col < cols {
            let cell = cells[col];
            if cell.spacer || cell.ch == ' ' {
                col += 1;
                continue;
            }
            let start = col;
            let mut text = String::new();
            while col < cols {
                let k = cells[col];
                if k.spacer || k.fg != cell.fg || k.bold != cell.bold || (k.ch != ' ' && k.ch.len_utf8() > 1 && col != start) {
                    break;
                }
                text.push(k.ch);
                col += 1;
                if k.ch.len_utf8() > 1 {
                    break;
                }
            }
            let text = text.trim_end().to_string();
            let run_font = if cell.bold { theme::mono_medium(FONT_SIZE) } else { font.clone() };
            let pos = pos2(inner.min.x + start as f32 * cell_w, y);
            painter.text(pos, Align2::LEFT_TOP, &text, run_font, c(cell.fg));
            if cell.underline {
                let w = text.chars().count() as f32 * cell_w;
                painter.line_segment([pos2(pos.x, y + cell_h - 2.0), pos2(pos.x + w, y + cell_h - 2.0)], Stroke::new(1.0, c(cell.fg)));
            }
        }
    }
    if let Some((cx, cy)) = snap.cursor {
        let r = Rect::from_min_size(pos2(inner.min.x + cx as f32 * cell_w, inner.min.y + cy as f32 * cell_h), vec2(cell_w, cell_h));
        if focused {
            painter.rect_filled(r, CornerRadius::same(1), theme::ca(theme::WORKING, 220));
            let ch = snap.cells[cy as usize * cols + cx as usize].ch;
            if ch != ' ' {
                painter.text(r.min, Align2::LEFT_TOP, ch.to_string(), font.clone(), c(theme::TERMINAL_BG));
            }
        } else {
            painter.rect_stroke(r, CornerRadius::same(1), Stroke::new(1.0, c(theme::WORKING)), StrokeKind::Inside);
        }
    }
    if snap.scroll_offset > 0 {
        painter.text(
            pos2(rect.max.x - 12.0, rect.min.y + 8.0),
            Align2::RIGHT_TOP,
            format!("SCROLLED {} LINES", snap.scroll_offset),
            theme::mono(10.0),
            Color32::from_gray(150),
        );
    }
    if !term.is_alive() {
        painter.text(rect.center_bottom() - vec2(0.0, 18.0), Align2::CENTER_BOTTOM, "shell exited", theme::mono(11.0), c(theme::TEXT_2));
    }
    focused
}

fn forward_input(ui: &mut Ui, term: &Terminal, app_cursor: bool) {
    let events = ui.input(|i| i.events.clone());
    for event in events {
        match event {
            Event::Text(text) => term.write(text.as_bytes()),
            Event::Paste(text) => term.paste(&text),
            // egui turns Ctrl+C and Ctrl+X into clipboard events. In a terminal they are control codes.
            Event::Copy => term.write(&[0x03]),
            Event::Cut => term.write(&[0x18]),
            Event::Key { key, pressed: true, modifiers, .. } => {
                let mods = Mods { ctrl: modifiers.ctrl, alt: modifiers.alt, shift: modifiers.shift };
                if let Some(k) = map_key(key, mods)
                    && let Some(bytes) = meerora_term::encode_key(k, mods, app_cursor)
                {
                    term.write(&bytes);
                }
            }
            _ => {}
        }
    }
}

fn map_key(key: EKey, mods: Mods) -> Option<Key> {
    Some(match key {
        EKey::Enter => Key::Enter,
        EKey::Backspace => Key::Backspace,
        EKey::Tab => Key::Tab,
        EKey::Escape => Key::Escape,
        EKey::ArrowUp => Key::Up,
        EKey::ArrowDown => Key::Down,
        EKey::ArrowLeft => Key::Left,
        EKey::ArrowRight => Key::Right,
        EKey::Home => Key::Home,
        EKey::End => Key::End,
        EKey::PageUp => Key::PageUp,
        EKey::PageDown => Key::PageDown,
        EKey::Insert => Key::Insert,
        EKey::Delete => Key::Delete,
        EKey::F1 => Key::F(1),
        EKey::F2 => Key::F(2),
        EKey::F3 => Key::F(3),
        EKey::F4 => Key::F(4),
        EKey::F5 => Key::F(5),
        EKey::F6 => Key::F(6),
        EKey::F7 => Key::F(7),
        EKey::F8 => Key::F(8),
        EKey::F9 => Key::F(9),
        EKey::F10 => Key::F(10),
        EKey::F11 => Key::F(11),
        EKey::F12 => Key::F(12),
        // Plain characters arrive as text. Only Ctrl or Alt combinations are keys.
        _ if mods.ctrl || mods.alt => {
            let name = key.name();
            let mut chars = name.chars();
            match (chars.next(), chars.next()) {
                (Some(ch), None) => Key::Char(ch.to_ascii_lowercase()),
                _ if key == EKey::Space => Key::Char(' '),
                _ => return None,
            }
        }
        _ => return None,
    })
}
