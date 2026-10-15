//! Alt+Tab switches between agents, like apps on a desktop. A quick press
//! goes back to the agent you viewed before. While Alt stays down, a strip
//! with every agent shows, each Tab moves on (Shift+Tab back), and letting go
//! of Alt opens the chosen agent. Esc keeps the current view. The order is
//! the most recently viewed first, then the rest in the order of the rail.
//! The tabs of one agent's view cycle with Alt+` instead (ui/mod.rs).

use egui::{Align2, CornerRadius, Id, LayerId, Order, Pos2, Rect, Stroke, StrokeKind, Ui, pos2, vec2};

use super::{View, open_agent, portrait};
use crate::engine::Core;
use crate::theme::{self, c};

/// Agents in the strip at most, so it fits the window.
const MAX_SHOWN: usize = 12;
const CELL: f32 = 112.0;

/// The switcher while Alt is held.
#[derive(Clone, Debug, PartialEq)]
pub struct Switcher {
    /// Agent ids in the order of the strip.
    pub order: Vec<String>,
    /// The chosen one.
    pub at: usize,
}

/// Records that the user looked at agent `id`, for the order of Alt+Tab.
pub fn note_viewed(recent: &mut Vec<String>, id: &str) {
    recent.retain(|r| r != id);
    recent.insert(0, id.to_string());
    recent.truncate(64);
}

/// The strip's order: the agents viewed most recently first, the others in
/// the order of `all`. Agents that are gone are left out.
pub fn order(recent: &[String], all: &[String]) -> Vec<String> {
    let mut out: Vec<String> = recent.iter().filter(|r| all.contains(r)).cloned().collect();
    let rest: Vec<String> = all.iter().filter(|a| !out.contains(a)).cloned().collect();
    out.extend(rest);
    out
}

/// Handles Alt+Tab, Alt+Shift+Tab, Esc and the release of Alt. Returns true
/// while the switcher is open, so the caller can leave other keys alone.
pub fn handle(ui: &mut Ui, core: &mut Core) -> bool {
    let (back, forward) = ui.input_mut(|i| {
        (i.consume_key(egui::Modifiers::ALT | egui::Modifiers::SHIFT, egui::Key::Tab), i.consume_key(egui::Modifiers::ALT, egui::Key::Tab))
    });
    if back || forward {
        let step: isize = if back { -1 } else { 1 };
        match &mut core.ui.switcher {
            Some(s) => s.at = (s.at as isize + step).rem_euclid(s.order.len() as isize) as usize,
            None => {
                let all: Vec<String> = core.world.agents.iter().map(|a| a.id.clone()).collect();
                let mut order = order(&core.ui.recent_agents, &all);
                // In an agent's view that agent leads, so one Tab goes to the one before it.
                if let View::Agent(current) = &core.ui.view {
                    order.retain(|id| id != current);
                    order.insert(0, current.clone());
                }
                if order.len() > 1 || !matches!(core.ui.view, View::Agent(_)) {
                    let start = if matches!(core.ui.view, View::Agent(_)) { 0 } else { usize::MAX };
                    let at = match (start, step) {
                        (usize::MAX, 1) => 0,
                        (usize::MAX, _) => order.len().saturating_sub(1),
                        (_, 1) => 1 % order.len(),
                        _ => order.len() - 1,
                    };
                    if !order.is_empty() {
                        core.ui.switcher = Some(Switcher { order, at });
                    }
                }
            }
        }
        // The keyboard comes back to meerora from an app window, like before.
        core.desktops.focus(None);
    }
    let Some(s) = core.ui.switcher.clone() else { return false };
    if ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape) || i.consume_key(egui::Modifiers::ALT, egui::Key::Escape)) {
        core.ui.switcher = None;
        return true;
    }
    if !ui.input(|i| i.modifiers.alt) {
        core.ui.switcher = None;
        if let Some(id) = s.order.get(s.at)
            && !matches!(&core.ui.view, View::Agent(current) if current == id)
        {
            open_agent(core, id);
        }
        return false;
    }
    draw(ui, core, &s);
    true
}

/// The strip in the middle of the window: portraits and names, the chosen
/// one framed.
fn draw(ui: &mut Ui, core: &Core, s: &Switcher) {
    let screen = ui.ctx().content_rect();
    let first = s.at.saturating_sub(MAX_SHOWN - 1).min(s.order.len().saturating_sub(MAX_SHOWN));
    let shown: Vec<(usize, &String)> = s.order.iter().enumerate().skip(first).take(MAX_SHOWN).collect();
    let width = CELL * shown.len() as f32 + 24.0;
    let rect = Rect::from_center_size(screen.center(), vec2(width, CELL + 52.0));
    let painter = ui.ctx().layer_painter(LayerId::new(Order::Tooltip, Id::new("agent-switcher")));
    painter.rect_filled(screen, CornerRadius::ZERO, egui::Color32::from_black_alpha(90));
    super::panel(&painter, rect, 18);
    for (k, (i, id)) in shown.iter().enumerate() {
        let cell = Rect::from_min_size(pos2(rect.min.x + 12.0 + CELL * k as f32, rect.min.y + 12.0), vec2(CELL - 8.0, CELL + 4.0));
        if *i == s.at {
            painter.rect(cell, CornerRadius::same(14), c([0x1A, 0x1F, 0x2A]), Stroke::new(2.0, c(theme::NEEDS_YOU)), StrokeKind::Inside);
        }
        let Some(a) = core.world.agent(id) else { continue };
        portrait(&painter, Rect::from_center_size(pos2(cell.center().x, cell.min.y + 42.0), vec2(64.0, 64.0)), core, a);
        let name = super::fit(&painter, &a.name, theme::ui_semibold(13.5), cell.width() - 10.0);
        painter.text(Pos2::new(cell.center().x, cell.max.y - 26.0), Align2::CENTER_CENTER, name, theme::ui_semibold(13.5), c(theme::TEXT));
        let (status, color) = super::status(a);
        painter.text(Pos2::new(cell.center().x, cell.max.y - 10.0), Align2::CENTER_CENTER, status, theme::mono(9.5), c(color));
    }
    painter.text(
        pos2(rect.center().x, rect.max.y - 14.0),
        Align2::CENTER_CENTER,
        "Tab next  ·  Shift+Tab back  ·  let go of Alt to switch  ·  Esc stays",
        theme::mono(10.0),
        c(theme::TEXT_2),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

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

    /// Alt+Tab with a real `Core`: a quick press goes back to the agent
    /// viewed before, held Alt with two Tabs walks on, Esc keeps the view.
    #[test]
    fn alt_tab_switches_agents() {
        let gpu = match meerora_scene::Gpu::headless() {
            Ok(gpu) => gpu,
            Err(e) => return eprintln!("skipped: no headless GPU ({e:#})"),
        };
        let assets = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets");
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().subsec_nanos();
        let data = std::env::temp_dir().join(format!("meerora-switcher-{}-{nanos}", std::process::id()));
        copy_dir(&assets.join("spaces"), &data.join("spaces"));
        for agent in ["wren", "claude", "quill"] {
            copy_dir(&assets.join("agents").join(agent), &data.join("agents").join(agent));
            // Only the chat, so no shell or Chrome starts.
            let toml = data.join("agents").join(agent).join("agent.toml");
            let text = std::fs::read_to_string(&toml).unwrap();
            let home = text.lines().find(|l| l.starts_with("home = ")).unwrap().to_string();
            std::fs::write(&toml, text.replacen(&home, &format!("{home}\napps = [\"chat\"]"), 1)).unwrap();
        }
        let assets = crate::assets::Assets::without_watcher(data.clone());
        let mut core = Core::new(&gpu.device, meerora_scene::IMAGE_FORMAT, assets, None, std::sync::Arc::new(|| {}));
        core.load(&gpu.device, &gpu.queue);
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        let screen = Rect::from_min_size(Pos2::ZERO, vec2(1440.0, 900.0));
        let mut time = 0.0;
        let mut frame = |core: &mut Core, events: Vec<egui::Event>, alt: bool| {
            time += 1.0 / 30.0;
            core.step(&gpu.device, &gpu.queue, 1.0 / 30.0);
            // egui-winit reports the held modifiers as their own event.
            let modifiers = if alt { egui::Modifiers::ALT } else { egui::Modifiers::NONE };
            let mut all = vec![egui::Event::ModifiersChanged(modifiers)];
            all.extend(events);
            let raw = egui::RawInput { screen_rect: Some(screen), time: Some(time), events: all, ..Default::default() };
            let out = ctx.run_ui(raw, |ui| core.build_ui(ui));
            core.apply_textures(&gpu.device, &gpu.queue, out.textures_delta);
        };
        let key = |key: egui::Key, modifiers: egui::Modifiers| {
            let e = |pressed| egui::Event::Key { key, physical_key: Some(key), pressed, repeat: false, modifiers };
            vec![e(true), e(false)]
        };
        let tab = || key(egui::Key::Tab, egui::Modifiers::ALT);
        let view = |core: &Core| match &core.ui.view {
            View::Agent(id) => id.clone(),
            other => format!("{other:?}"),
        };

        open_agent(&mut core, "claude");
        open_agent(&mut core, "wren");
        frame(&mut core, Vec::new(), false);
        // A quick Alt+Tab goes back to Claude.
        frame(&mut core, tab(), true);
        assert!(core.ui.switcher.is_some(), "the strip shows while Alt is held");
        frame(&mut core, Vec::new(), false);
        assert_eq!(view(&core), "claude");
        assert!(core.ui.switcher.is_none());
        // Holding Alt and pressing Tab twice skips Wren and reaches Quill, which was never viewed.
        frame(&mut core, tab(), true);
        frame(&mut core, tab(), true);
        frame(&mut core, Vec::new(), true);
        assert_eq!(core.ui.switcher.as_ref().map(|s| s.order[s.at].clone()).as_deref(), Some("quill"));
        frame(&mut core, Vec::new(), false);
        assert_eq!(view(&core), "quill");
        // Shift goes back, and Esc keeps the current view.
        frame(&mut core, key(egui::Key::Tab, egui::Modifiers::ALT | egui::Modifiers::SHIFT), true);
        frame(&mut core, key(egui::Key::Escape, egui::Modifiers::ALT), true);
        frame(&mut core, Vec::new(), false);
        assert_eq!(view(&core), "quill", "Esc stays");
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn the_most_recent_come_first_then_the_rail() {
        let all = ids(&["claude", "ember", "moth", "quill"]);
        assert_eq!(order(&ids(&["moth", "gone", "claude"]), &all), ids(&["moth", "claude", "ember", "quill"]));
        assert_eq!(order(&[], &all), all);
        let mut recent = ids(&["moth", "claude"]);
        note_viewed(&mut recent, "claude");
        assert_eq!(recent, ids(&["claude", "moth"]));
        note_viewed(&mut recent, "quill");
        assert_eq!(recent, ids(&["quill", "claude", "moth"]));
    }
}
