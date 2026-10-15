//! `meerora --session` runs meerora as a login session (D14, D15). The launcher
//! `scripts/meerora-session` runs it as the only app of the cage kiosk
//! compositor, so quitting meerora ends cage and with it the session.
//!
//! Window mode stays the default. The session differs in four points:
//! 1. It always runs fullscreen and borderless.
//! 2. It always asks for the desktop shortcuts, also with `inhibit_shortcuts = false`.
//! 3. It needs the agent daemon and starts it when none runs.
//! 4. It has a Log out button in the space header, and Ctrl+Alt+Delete asks
//!    the same question from every view while meerora has the keyboard.

use egui::text::LayoutJob;
use egui::{Align2, Color32, CornerRadius, Id, Key, LayerId, Modifiers, Order, Rect, Sense, Shape, Stroke, StrokeKind, Ui, pos2, vec2};

use crate::theme::{self, c, ca};
use crate::ui::{ButtonStyle, button};

/// State of the log out button and its question.
#[derive(Default)]
pub struct Session {
    /// The question "Log out of meerora?" is open.
    pub asking: bool,
    /// The user confirmed. The app leaves its event loop after this frame.
    pub quit: bool,
}

const BUTTON_ID: &str = "session-logout";
const CONFIRM_ID: &str = "session-logout-confirm";

impl Session {
    /// With the question already open, for snapshots.
    pub fn asking() -> Self {
        Session { asking: true, quit: false }
    }

    /// Draws the button (in the space view only) and the question. Call it
    /// before the view, so Esc and Enter answer the question and nothing behind it.
    pub fn draw(&mut self, ui: &mut Ui, in_space: bool) {
        // Consumed, so Delete does not also reach Files or the editor behind the question.
        if ui.input_mut(|i| i.consume_key(Modifiers::CTRL | Modifiers::ALT, Key::Delete)) {
            self.asking = true;
        }
        if in_space && !self.asking {
            self.draw_button(ui);
        }
        if self.asking {
            self.draw_question(ui);
        }
    }

    /// The pill right of the meerora name in the space header.
    fn draw_button(&mut self, ui: &mut Ui) {
        let name_w = ui.painter().layout_no_wrap("meerora".into(), theme::ui_bold(22.0), Color32::WHITE).size().x;
        let header_h = crate::ui::space::HEADER_H;
        let rect = Rect::from_min_size(pos2(60.0 + name_w + 20.0, header_h / 2.0 - 16.0), vec2(108.0, 32.0));
        let mut clicked = false;
        egui::Area::new(Id::new(BUTTON_ID)).fade_in(false).fixed_pos(rect.min).order(Order::Foreground).show(ui.ctx(), |ui| {
            ui.allocate_exact_size(rect.size(), Sense::hover());
            let resp = ui.interact(rect, Id::new("session-logout-click"), Sense::click());
            let painter = ui.painter();
            let fill = if resp.hovered() { c([0x1A, 0x1F, 0x2A]) } else { c(theme::PANEL) };
            painter.rect(rect, CornerRadius::same(16), fill, Stroke::new(1.0, c(theme::BORDER)), StrokeKind::Inside);
            power_icon(painter, pos2(rect.min.x + 20.0, rect.center().y), 6.0, Stroke::new(1.4, c(theme::TEXT_2)));
            painter.text(pos2(rect.min.x + 34.0, rect.center().y), Align2::LEFT_CENTER, "Log out", theme::ui(14.0), c(theme::TEXT));
            if resp.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
            clicked = resp.clicked();
        });
        if clicked {
            self.asking = true;
        }
    }

    /// The question over everything else. Enter logs out, Esc cancels.
    fn draw_question(&mut self, ui: &mut Ui) {
        let screen = ui.ctx().content_rect();
        let w = 520.0_f32.min(screen.width() - 48.0);
        let text_w = w - 48.0;
        let body = "Your agents stop and the apps on their desktops close. Their files and memory stay. The login screen comes back.";
        let mut job = LayoutJob::simple(body.into(), theme::ui(14.5), c([0xC9, 0xCE, 0xD9]), text_w);
        job.wrap.break_anywhere = false;
        let galley = ui.painter().layout_job(job);
        let h = 24.0 + 26.0 + 12.0 + galley.size().y + 24.0 + 44.0 + 22.0;
        let rect = Rect::from_center_size(screen.center(), vec2(w, h));
        let mut answer = None;
        egui::Area::new(Id::new(CONFIRM_ID)).fade_in(false).fixed_pos(screen.min).order(Order::Foreground).show(ui.ctx(), |ui| {
            // The whole screen takes the pointer, so nothing behind the card reacts.
            ui.allocate_exact_size(screen.size(), Sense::click_and_drag());
            let painter = ui.painter();
            painter.rect_filled(screen, CornerRadius::ZERO, Color32::from_black_alpha(150));
            let shadow = egui::epaint::Shadow { offset: [0, 18], blur: 44, spread: 0, color: Color32::from_black_alpha(140) };
            painter.add(shadow.as_shape(rect, CornerRadius::same(18)));
            painter.rect(rect, CornerRadius::same(18), c(theme::PANEL_2), Stroke::new(1.0, ca(theme::NEEDS_YOU, 90)), StrokeKind::Inside);
            let x = rect.min.x + 24.0;
            power_icon(painter, pos2(x + 9.0, rect.min.y + 26.0 + 12.0), 8.0, Stroke::new(1.8, c(theme::NEEDS_YOU)));
            painter.text(pos2(x + 30.0, rect.min.y + 26.0), Align2::LEFT_TOP, "Log out of meerora?", theme::ui_bold(18.0), c(theme::TEXT));
            painter.galley(pos2(x, rect.min.y + 26.0 + 26.0 + 12.0), galley, c([0xC9, 0xCE, 0xD9]));
            let cancel = Rect::from_min_size(pos2(rect.max.x - 24.0 - 104.0, rect.max.y - 22.0 - 44.0), vec2(104.0, 44.0));
            let yes = Rect::from_min_size(pos2(cancel.min.x - 12.0 - 120.0, cancel.min.y), vec2(120.0, 44.0));
            if button(ui, yes, "Log out", ButtonStyle::Primary).clicked() {
                answer = Some(true);
            }
            if button(ui, cancel, "Cancel", ButtonStyle::Quiet).clicked() {
                answer = Some(false);
            }
        });
        // Above the talk field and the other foreground areas of the views.
        ui.ctx().move_to_top(LayerId::new(Order::Foreground, Id::new(CONFIRM_ID)));
        if ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
            answer = Some(false);
        }
        if ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter)) {
            answer = Some(true);
        }
        match answer {
            Some(true) => {
                tracing::info!("logging out of the meerora session");
                self.asking = false;
                self.quit = true;
            }
            Some(false) => self.asking = false,
            None => {}
        }
    }
}

/// The power symbol: a ring open at the top with a bar through the gap.
fn power_icon(painter: &egui::Painter, center: egui::Pos2, r: f32, stroke: Stroke) {
    let gap = 40.0_f32.to_radians();
    let start = -std::f32::consts::FRAC_PI_2 + gap;
    let sweep = std::f32::consts::TAU - 2.0 * gap;
    let points = (0..=24).map(|i| {
        let a = start + sweep * i as f32 / 24.0;
        center + vec2(a.cos(), a.sin()) * r
    });
    painter.add(Shape::line(points.collect(), stroke));
    painter.line_segment([center - vec2(0.0, r + 1.5), center - vec2(0.0, r * 0.15)], stroke);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs one egui pass with the given key presses.
    fn pass(ctx: &egui::Context, session: &mut Session, in_space: bool, keys: &[(Key, Modifiers)]) {
        let mut raw = egui::RawInput { screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(1440.0, 900.0))), ..Default::default() };
        for (key, modifiers) in keys {
            raw.events.push(egui::Event::Key { key: *key, physical_key: None, pressed: true, repeat: false, modifiers: *modifiers });
        }
        let mut out = ctx.run_ui(raw, |ui| session.draw(ui, in_space));
        // There is no renderer to take the font atlas.
        out.textures_delta.clear();
    }

    #[test]
    fn ctrl_alt_delete_asks_and_enter_logs_out() {
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        let mut session = Session::default();
        pass(&ctx, &mut session, false, &[]);
        assert!(!session.asking && !session.quit);
        pass(&ctx, &mut session, false, &[(Key::Delete, Modifiers::CTRL | Modifiers::ALT)]);
        assert!(session.asking && !session.quit);
        pass(&ctx, &mut session, false, &[(Key::Enter, Modifiers::NONE)]);
        assert!(!session.asking && session.quit);
    }

    #[test]
    fn escape_keeps_the_session() {
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        let mut session = Session::asking();
        pass(&ctx, &mut session, true, &[(Key::Escape, Modifiers::NONE)]);
        assert!(!session.asking && !session.quit);
    }
}
