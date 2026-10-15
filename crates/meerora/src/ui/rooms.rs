//! The room picker (D6): a click on the space card in the sidebar lists the
//! rooms, the workshop with its garden, the space station, the office and
//! the user's own `space/` folder as "Custom" when it exists.
//!
//! Choosing a room writes `[space] room` into `meerora.toml` and copies the
//! room's template into the data dir the first time
//! (`meerora_protocol::set_room`). It never changes the files of another
//! room, so edits survive every switch. The app then builds the new space,
//! its devices and the graveyard plot, and the daemon reloads its stations
//! and devices (`ReloadAgents`).

use egui::{Align2, Color32, CornerRadius, Id, Order, Painter, Pos2, Rect, Sense, Shape, Stroke, StrokeKind, Ui, pos2, vec2};
use meerora_protocol::{Command, Room};

use super::space::HEADER_H;
use super::{ButtonStyle, button, fit, label, panel};
use crate::engine::Core;
use crate::theme::{self, c, ca};

const CARD_W: f32 = 620.0;
const ROW_H: f32 = 84.0;

/// The rooms while the picker is open.
fn key() -> Id {
    Id::new("room-picker")
}

/// Opens the picker with the rooms of the data dir.
pub fn open(ctx: &egui::Context, core: &Core) {
    let rooms = meerora_protocol::rooms(&core.assets.data_dir);
    ctx.data_mut(|d| d.insert_temp(key(), rooms));
}

pub fn close(ctx: &egui::Context) {
    ctx.data_mut(|d| d.remove::<Vec<Room>>(key()));
}

/// The name of the room the space is, for the sidebar card. It reads
/// `meerora.toml` at most once a second.
pub fn current_name(ctx: &egui::Context, core: &Core) -> String {
    let now = ctx.input(|i| i.time);
    let id = Id::new("room-name");
    if let Some((at, name)) = ctx.data(|d| d.get_temp::<(f64, String)>(id))
        && now - at < 1.0
    {
        return name;
    }
    let name = room_name(&meerora_protocol::current_room(&core.assets.data_dir));
    ctx.data_mut(|d| d.insert_temp(id, (now, name.clone())));
    name
}

fn room_name(room: &str) -> String {
    match meerora_protocol::TEMPLATES.iter().find(|t| t.id == room) {
        Some(t) => t.name.to_string(),
        None if room == meerora_protocol::CUSTOM_ROOM => "Custom space".into(),
        None => room.replace(['-', '_'], " "),
    }
}

/// Switches to `room` and rebuilds what depends on it. Returns what to tell the user.
pub fn choose(core: &mut Core, room: &str) -> Result<String, String> {
    meerora_protocol::set_room(&core.assets.data_dir, room)?;
    core.assets.reload_space();
    if let Some(link) = &core.link {
        // The daemon reads the stations and the devices of the new room.
        link.send(&Command::ReloadAgents);
    }
    Ok(format!("The space is the {} now", room_name(room).to_lowercase()))
}

/// The picker over the space view, while it is open.
pub fn draw(ui: &mut Ui, core: &mut Core, screen: Rect) {
    let Some(rooms) = ui.ctx().data(|d| d.get_temp::<Vec<Room>>(key())) else { return };
    let h = 112.0 + rooms.len() as f32 * (ROW_H + 10.0) + 64.0;
    let rect = Rect::from_center_size(pos2(screen.center().x, screen.center().y), vec2(CARD_W, h));
    let rect = rect.translate(vec2(0.0, (HEADER_H + 16.0 - rect.min.y).max(0.0)));
    let mut chosen: Option<String> = None;
    let mut dismiss = false;

    // The dim layer and the card share one area. As two areas of the same
    // order, a click could bring the shade over the card, which then looked
    // greyed out and passed every click to the shade.
    egui::Area::new(Id::new("room-picker-area")).fade_in(false).fixed_pos(Pos2::ZERO).order(Order::Foreground).show(ui.ctx(), |ui| {
        let shade = ui.allocate_rect(screen, Sense::click());
        ui.painter().rect_filled(screen, CornerRadius::ZERO, Color32::from_black_alpha(110));
        if shade.clicked() && shade.interact_pointer_pos().is_some_and(|p| !rect.contains(p)) {
            dismiss = true;
        }
        // The card's background takes clicks that miss a row.
        ui.interact(rect, Id::new("room-picker-card"), Sense::click());
        let painter = ui.painter().clone();
        let shadow = egui::epaint::Shadow { offset: [0, 18], blur: 48, spread: 0, color: Color32::from_black_alpha(140) };
        painter.add(shadow.as_shape(rect, CornerRadius::same(18)));
        panel(&painter, rect, 18);
        let x = rect.min.x + 24.0;
        let inner = rect.width() - 48.0;
        let mut y = rect.min.y + 22.0;
        label(&painter, pos2(x, y), "ROOM");
        y += 20.0;
        painter.text(pos2(x, y), Align2::LEFT_TOP, "Where your agents live", theme::ui_bold(19.0), c(theme::TEXT));
        y += 28.0;
        let note = "Every room has the same stations, so homes and tools keep working. A room is copied into your data folder \
                    the first time you pick it, and your edits stay in its folder when you switch.";
        let galley = wrapped(&painter, note, theme::ui(12.5), c(theme::TEXT_2), inner, 2);
        let gh = galley.size().y;
        painter.galley(pos2(x, y), galley, c(theme::TEXT_2));
        y += gh + 16.0;

        for room in &rooms {
            let row = Rect::from_min_size(pos2(x, y), vec2(inner, ROW_H));
            let resp = ui.interact(row, Id::new(("room-row", room.id.as_str())), Sense::click());
            let painter = ui.painter();
            let hovered = resp.hovered() && !room.current;
            let (fill, stroke) = if room.current {
                (ca(theme::NEEDS_YOU, 18), Stroke::new(1.0, ca(theme::NEEDS_YOU, 170)))
            } else if hovered {
                (c([0x1A, 0x1F, 0x2A]), Stroke::new(1.0, c([0x3A, 0x42, 0x52])))
            } else {
                (c([0x16, 0x1A, 0x23]), Stroke::new(1.0, c([0x23, 0x28, 0x34])))
            };
            painter.rect(row, CornerRadius::same(12), fill, stroke, StrokeKind::Inside);
            let icon = Rect::from_center_size(pos2(row.min.x + 38.0, row.center().y), vec2(52.0, 52.0));
            draw_icon(painter, &room.id, icon);
            let text_x = row.min.x + 76.0;
            let mut right = row.max.x - 14.0;
            if room.current {
                right = super::pill(painter, pos2(right, row.min.y + 22.0), "CURRENT", theme::NEEDS_YOU).min.x - 8.0;
            }
            let name = fit(painter, &room.name, theme::ui_semibold(16.0), right - text_x);
            painter.text(pos2(text_x, row.min.y + 12.0), Align2::LEFT_TOP, name, theme::ui_semibold(16.0), c(theme::TEXT));
            let sub = match room.same_as.as_deref() {
                Some(same) if room.id == meerora_protocol::CUSTOM_ROOM => {
                    format!("{} Its scripts are the same as the {} template.", room.description, room_name(same).to_lowercase())
                }
                _ => room.description.clone(),
            };
            let galley = wrapped(painter, &sub, theme::ui(12.5), c(theme::TEXT_2), row.max.x - 14.0 - text_x, 2);
            painter.galley(pos2(text_x, row.min.y + 38.0), galley, c(theme::TEXT_2));
            if hovered {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
            if resp.clicked() && !room.current {
                chosen = Some(room.id.clone());
            }
            y += ROW_H + 10.0;
        }
        y += 6.0;
        let close = Rect::from_min_size(pos2(rect.max.x - 24.0 - 110.0, y), vec2(110.0, 38.0));
        if button(ui, close, "Close", ButtonStyle::Quiet).clicked() {
            dismiss = true;
        }
    });
    if ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
        dismiss = true;
    }
    let now = core.world.time;
    if let Some(room) = chosen {
        match choose(core, &room) {
            Ok(text) => {
                core.ui.toast(now, text, false);
                ui.ctx().data_mut(|d| d.remove::<(f64, String)>(Id::new("room-name")));
                close(ui.ctx());
            }
            Err(e) => core.ui.toast(now, format!("The room did not change: {e}"), true),
        }
    } else if dismiss {
        close(ui.ctx());
    }
}

/// `text` wrapped to `width`, at most `rows` rows with an ellipsis.
fn wrapped(painter: &Painter, text: &str, font: egui::FontId, color: Color32, width: f32, rows: usize) -> std::sync::Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::simple(text.to_string(), font, color, width);
    job.wrap.max_rows = rows;
    job.wrap.overflow_character = Some('…');
    painter.layout_job(job)
}

/// A small picture of a room: a house with a garden, a ring module, an office.
pub fn draw_icon(painter: &Painter, room: &str, rect: Rect) {
    let p = |x: f32, y: f32| pos2(rect.center().x + x * rect.width() / 52.0, rect.center().y + y * rect.height() / 52.0);
    let poly = |pts: Vec<Pos2>, color: [u8; 3]| Shape::convex_polygon(pts, c(color), Stroke::NONE);
    painter.rect_filled(rect, CornerRadius::same(10), c([0x10, 0x13, 0x1A]));
    match room {
        "workshop" => {
            painter.add(poly(vec![p(-20.0, 4.0), p(0.0, -6.0), p(20.0, 4.0), p(0.0, 14.0)], [0x4F, 0x7A, 0x45]));
            painter.add(poly(vec![p(-20.0, 4.0), p(-6.0, -3.0), p(8.0, 4.0), p(-6.0, 11.0)], [0x9A, 0x6A, 0x44]));
            painter.add(poly(vec![p(-20.0, 4.0), p(-20.0, -14.0), p(-6.0, -21.0), p(-6.0, -3.0)], [0xE9, 0xDC, 0xC6]));
            painter.add(poly(vec![p(-6.0, -3.0), p(-6.0, -21.0), p(8.0, -14.0), p(8.0, 4.0)], [0xD8, 0xC8, 0xAE]));
            painter.circle_filled(p(12.0, -4.0), 5.0 * rect.width() / 52.0, c([0x3E, 0x7A, 0x44]));
        }
        "space-station" => {
            let r = 15.0 * rect.width() / 52.0;
            painter.circle_stroke(p(0.0, 0.0), r, Stroke::new(5.0 * rect.width() / 52.0, c([0x8A, 0x94, 0xA6])));
            painter.circle_filled(p(0.0, 0.0), 4.0 * rect.width() / 52.0, c([0x7F, 0xE3, 0xD0]));
            painter.line_segment([p(-22.0, 0.0), p(22.0, 0.0)], Stroke::new(2.0, c([0x5A, 0x63, 0x75])));
            painter.circle_filled(p(-12.0, -15.0), 1.4, c([0xFF, 0xFF, 0xFF]));
            painter.circle_filled(p(16.0, 14.0), 1.2, c([0xFF, 0xFF, 0xFF]));
        }
        "office" => {
            painter.add(poly(vec![p(-20.0, 4.0), p(0.0, -6.0), p(20.0, 4.0), p(0.0, 14.0)], [0x5A, 0x63, 0x75]));
            painter.add(poly(vec![p(-8.0, 2.0), p(2.0, -3.0), p(12.0, 2.0), p(2.0, 7.0)], [0xC8, 0xA2, 0x6A]));
            painter.add(poly(vec![p(-3.0, -1.0), p(-3.0, -10.0), p(5.0, -14.0), p(5.0, -5.0)], [0x2A, 0x30, 0x3D]));
            painter.add(poly(vec![p(-2.0, -2.0), p(-2.0, -9.0), p(4.0, -12.0), p(4.0, -6.0)], [0x8C, 0xD3, 0xFF]));
            painter.circle_filled(p(-13.0, -6.0), 5.0 * rect.width() / 52.0, c([0x4F, 0x8A, 0x4A]));
        }
        _ => {
            painter.add(poly(vec![p(-18.0, 4.0), p(0.0, -5.0), p(18.0, 4.0), p(0.0, 13.0)], [0x7A, 0x5A, 0x9C]));
            painter.add(poly(vec![p(-6.0, -2.0), p(-6.0, -16.0), p(6.0, -16.0), p(6.0, -2.0)], [0xE8, 0xEA, 0xF0]));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn copy_dir(from: &std::path::Path, to: &std::path::Path) {
        std::fs::create_dir_all(to).unwrap();
        for entry in std::fs::read_dir(from).unwrap().flatten() {
            let target = to.join(entry.file_name());
            if entry.path().is_dir() {
                copy_dir(&entry.path(), &target);
            } else {
                std::fs::copy(entry.path(), target).unwrap();
            }
        }
    }

    /// Clicks go to the rooms, also after a first click on the card. The
    /// shade and the card were two areas, and a click could raise the shade
    /// over the card, so every room looked greyed out and could not be picked.
    #[test]
    fn a_click_on_a_room_picks_it() {
        let gpu = match meerora_scene::Gpu::headless() {
            Ok(gpu) => gpu,
            Err(e) => return eprintln!("skipped: no headless GPU ({e:#})"),
        };
        let assets = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets");
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().subsec_nanos();
        let data = std::env::temp_dir().join(format!("meerora-rooms-{}-{nanos}", std::process::id()));
        copy_dir(&assets.join("spaces"), &data.join("spaces"));
        let mut core = Core::new(
            &gpu.device,
            meerora_scene::IMAGE_FORMAT,
            crate::assets::Assets::without_watcher(data.clone()),
            None,
            std::sync::Arc::new(|| {}),
        );
        core.load(&gpu.device, &gpu.queue);
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        let screen = Rect::from_min_size(Pos2::ZERO, vec2(1440.0, 900.0));
        let mut time = 0.0;
        let mut frame = |core: &mut Core, events: Vec<egui::Event>| {
            time += 1.0 / 30.0;
            core.step(&gpu.device, &gpu.queue, 1.0 / 30.0);
            let raw = egui::RawInput { screen_rect: Some(screen), time: Some(time), events, ..Default::default() };
            let out = ctx.run_ui(raw, |ui| core.build_ui(ui));
            core.apply_textures(&gpu.device, &gpu.queue, out.textures_delta);
        };
        let click = |pos: Pos2| {
            let b = |pressed| egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            };
            vec![egui::Event::PointerMoved(pos), b(true), b(false)]
        };
        frame(&mut core, Vec::new());
        open(&ctx, &core);
        frame(&mut core, Vec::new());
        assert_eq!(meerora_protocol::current_room(&data), "workshop");
        // Walk down the middle of the window, one click per step, until a room
        // other than the current one is picked. A click outside the card
        // closes the picker, so it opens again for the next step.
        let is_open = |ctx: &egui::Context| ctx.data(|d| d.get_temp::<Vec<Room>>(key()).is_some());
        let (mut picked, mut kept_open) = (None, 0);
        for step in 0..46 {
            if !is_open(&ctx) {
                open(&ctx, &core);
                frame(&mut core, Vec::new());
            }
            let pos = pos2(screen.center().x, HEADER_H + 20.0 + step as f32 * 18.0);
            frame(&mut core, click(pos));
            frame(&mut core, Vec::new());
            let room = meerora_protocol::current_room(&data);
            if room != "workshop" {
                picked = Some(room);
                break;
            }
            kept_open += usize::from(is_open(&ctx));
        }
        assert_eq!(picked.as_deref(), Some("space-station"), "a click on the row after the current workshop picks it");
        assert!(kept_open >= 3, "clicks on the card keep the picker open ({kept_open})");
        let _ = std::fs::remove_dir_all(&data);
    }
}
