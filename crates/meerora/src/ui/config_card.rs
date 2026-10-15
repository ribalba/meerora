//! The card for the helper's configuration changes (`config_change`).
//!
//! The helper (`role = "helper"`, Rivet on a new install) proposes the whole
//! new content of `meerora.toml`, `commands.toml` or an `agent.toml`, or a
//! new agent, and the daemon sends it as `Event::ConfigProposal` with a
//! unified diff. The card says who proposes what and why, shows the diff
//! and has Apply and Discard. There is no "always" for configuration
//! changes. The daemon writes nothing before Apply.
//!
//! Granting a capability through a diff is allowed only because the user
//! reads it here, so changed lines that touch what an agent may do or how it
//! runs are highlighted in the warning colour: `granted`, `system`,
//! `workspace`, `role`, the runtime, the `[sandbox]`, `[paths]` and
//! `[builder]` tables of `meerora.toml`, and `launch` of `commands.toml`.
//!
//! The card floats over every view at the bottom of the screen. The daemon
//! closes a proposal with `ConfigResult`, and its greeting sends the open
//! ones again. A failed Apply shows as a toast.

use std::collections::HashSet;
use std::sync::Arc;

use egui::text::LayoutJob;
use egui::{Align2, Color32, CornerRadius, FontId, Galley, Id, Order, Painter, Rect, Sense, Stroke, StrokeKind, Ui, pos2, vec2};
use meerora_protocol::{AgentRole, Command, Event};

use super::{ButtonStyle, button, fit, portrait};
use crate::engine::Core;
use crate::theme::{self, c, ca};

/// Height of one diff line.
const LINE_H: f32 = 19.0;
const ADD: [u8; 3] = [0x7E, 0xC6, 0x8F];
const DEL: [u8; 3] = [0xE0, 0x67, 0x5A];
const DIM: [u8; 3] = [0x8A, 0x92, 0xA3];
const BODY: [u8; 3] = [0xC9, 0xCE, 0xD9];
/// The warning colour for lines that change what an agent may do.
const WARN: [u8; 3] = theme::WORKING;
/// Tables of which every changed line is highlighted.
const SENSITIVE_TABLES: [&str; 5] = ["capabilities", "runtime", "sandbox", "paths", "builder"];
/// Keys whose changed lines are highlighted in any table.
/// `launch` of commands.toml starts a program as the user.
const SENSITIVE_KEYS: [&str; 8] = ["granted", "system", "workspace", "role", "runtime", "capabilities", "external", "launch"];

/// What a line of the diff is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// The file the following hunks change.
    File,
    /// `@@ -4,2 +4,3 @@ [capabilities]`
    Hunk,
    Context,
    Add,
    Del,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DiffLine {
    pub kind: Kind,
    /// Without the leading `+`, `-` or space.
    pub text: String,
    /// A changed line that touches what an agent may do or how it runs.
    pub warn: bool,
}

/// A proposal that waits for the user.
#[derive(Clone, Debug, PartialEq)]
pub struct Proposal {
    pub id: String,
    pub agent: String,
    pub target: String,
    pub path: String,
    pub reason: String,
    pub lines: Vec<DiffLine>,
}

#[derive(Debug, Default)]
pub struct ConfigCards {
    /// Open proposals in the order they came.
    pub open: Vec<Proposal>,
    /// Proposals the user applied whose result has not come yet.
    applying: HashSet<String>,
}

/// Keeps the cards in step with the daemon and reports the result of an Apply.
pub fn on_event(core: &mut Core, event: &Event) {
    if let Event::ConfigResult { id, applied, message } = event
        && core.ui.config_changes.applying.remove(id)
    {
        let text =
            message.clone().unwrap_or_else(|| if *applied { "The change is written.".into() } else { "Nothing was written.".into() });
        core.ui.toast(core.world.time, text, !applied);
    }
    apply(&mut core.ui.config_changes, event);
}

pub fn apply(state: &mut ConfigCards, event: &Event) {
    match event {
        // The greeting sends the open proposals again.
        Event::Hello { .. } => state.open.clear(),
        Event::ConfigProposal { id, agent, target, path, reason, diff } => {
            if !state.open.iter().any(|p| p.id == *id) {
                state.open.push(Proposal {
                    id: id.clone(),
                    agent: agent.clone(),
                    target: target.clone(),
                    path: path.clone(),
                    reason: reason.clone(),
                    lines: parse_diff(diff),
                });
            }
        }
        Event::ConfigResult { id, .. } => state.open.retain(|p| p.id != *id),
        Event::AgentRemoved { agent } => state.open.retain(|p| p.agent != *agent),
        _ => {}
    }
}

/// Whether `id` is meerora's helper, which never moves to the graveyard.
pub fn is_helper(core: &Core, id: &str) -> bool {
    core.agent_dir(id).is_some_and(|d| d.config.role == Some(AgentRole::Helper))
}

/// Reads the unified diff of a proposal. Hunk heads of TOML files name the
/// table they start in, and every header line on the way changes it.
pub fn parse_diff(diff: &str) -> Vec<DiffLine> {
    let mut out = Vec::new();
    let (mut old_left, mut new_left) = (0usize, 0usize);
    let (mut toml, mut created) = (false, false);
    let mut table = String::new();
    // A multi-line value of a sensitive key, such as `workspace = [`, is open.
    let mut open_value = false;
    for line in diff.lines() {
        if old_left == 0 && new_left == 0 {
            if let Some(from) = line.strip_prefix("--- ") {
                created = from.trim() == "/dev/null";
            } else if let Some(to) = line.strip_prefix("+++ ") {
                let path = to.trim().strip_prefix("b/").unwrap_or(to.trim());
                toml = path.ends_with(".toml");
                let text = if created { format!("{path}  (new file)") } else { path.to_string() };
                out.push(DiffLine { kind: Kind::File, text, warn: false });
            } else if let Some(head) = line.strip_prefix("@@ ") {
                let mut parts = head.splitn(3, ' ');
                let count = |part: Option<&str>| part.and_then(|p| p.split(',').nth(1)).and_then(|n| n.parse::<usize>().ok()).unwrap_or(0);
                old_left = count(parts.next());
                new_left = count(parts.next());
                let section = parts.next().unwrap_or("").trim_start_matches('@').trim();
                table = table_name(section).unwrap_or_default();
                open_value = false;
                out.push(DiffLine { kind: Kind::Hunk, text: line.to_string(), warn: false });
            }
            continue;
        }
        let (kind, body) = match line.as_bytes().first() {
            Some(b'+') => (Kind::Add, &line[1..]),
            Some(b'-') => (Kind::Del, &line[1..]),
            Some(b' ') => (Kind::Context, &line[1..]),
            _ => (Kind::Context, line),
        };
        match kind {
            Kind::Add => new_left = new_left.saturating_sub(1),
            Kind::Del => old_left = old_left.saturating_sub(1),
            _ => {
                old_left = old_left.saturating_sub(1);
                new_left = new_left.saturating_sub(1);
            }
        }
        let mut warn = false;
        if toml {
            let trimmed = body.trim();
            let changed = kind != Kind::Context;
            if let Some(name) = table_name(trimmed) {
                table = name;
                open_value = false;
                warn = changed && sensitive_table(&table);
            } else {
                let key = sensitive_key(trimmed);
                let plain = trimmed.is_empty() || trimmed.starts_with('#');
                warn = changed && ((sensitive_table(&table) && !plain) || key || open_value);
                let depth = trimmed.matches('[').count() as i32 - trimmed.matches(']').count() as i32;
                if key && depth > 0 {
                    open_value = true;
                } else if open_value && depth < 0 {
                    open_value = false;
                }
            }
        }
        out.push(DiffLine { kind, text: body.to_string(), warn });
    }
    out
}

/// `capabilities` for `[capabilities]`, `schedule` for `[[schedule]]`.
fn table_name(line: &str) -> Option<String> {
    let line = line.split('#').next().unwrap_or("").trim();
    (line.starts_with('[') && line.ends_with(']')).then(|| line.trim_matches(['[', ']']).trim().to_string())
}

fn sensitive_table(table: &str) -> bool {
    SENSITIVE_TABLES.contains(&table.split('.').next().unwrap_or("").trim())
}

fn sensitive_key(line: &str) -> bool {
    let Some((key, _)) = line.split_once('=') else { return false };
    SENSITIVE_KEYS.contains(&key.trim().trim_matches('"'))
}

/// The card title, for example "Rivet proposes a change to meerora.toml".
fn title(core: &Core, name: &str, target: &str) -> String {
    if let Some(id) = target.strip_prefix("new_agent:") {
        return format!("{name} proposes a new agent, {id}");
    }
    if let Some(id) = target.strip_prefix("agent:") {
        let agent = core.world.agent(id).map_or(id.to_string(), |a| a.name.clone());
        return format!("{name} proposes a change to the settings of {agent}");
    }
    format!("{name} proposes a change to {target}")
}

/// Sends the decision. The card goes away at once, the daemon confirms with `ConfigResult`.
fn decide(core: &mut Core, id: &str, apply: bool) {
    if let Some(link) = &core.link {
        link.send(&Command::DecideConfig { id: id.into(), apply });
    }
    core.ui.config_changes.open.retain(|p| p.id != id);
    if apply {
        core.ui.config_changes.applying.insert(id.to_string());
    }
}

/// The card for the first open proposal, over whatever view shows.
pub fn draw(ui: &mut Ui, core: &mut Core) {
    if !core.ui.daemon_connected {
        return;
    }
    let Some(p) = core.ui.config_changes.open.first().cloned() else { return };
    let more = core.ui.config_changes.open.len() - 1;
    let screen = ui.ctx().content_rect();
    // In the space it takes the place of the needs-you card right of the
    // sidebar, elsewhere it is centred.
    let (left, w) = if core.ui.view == super::View::Space {
        let left = super::space::sidebar_right() + 20.0;
        (left, (screen.max.x - 24.0 - left).max(320.0))
    } else {
        let w = (screen.width() - 48.0).clamp(320.0, 880.0);
        (screen.center().x - w / 2.0, w)
    };
    let inner = w - 40.0;
    let (name, system) = core.world.agent(&p.agent).map_or((p.agent.clone(), false), |a| (a.name.clone(), a.system));
    let accent = if system { theme::SYSTEM_2 } else { theme::NEEDS_YOU };
    let reason = paragraph(ui.painter(), &format!("\u{201C}{}\u{201D}", p.reason), theme::ui(14.5), c(BODY), inner, 3);
    let warn = p.lines.iter().any(|l| l.warn);
    let note_h = if warn { 34.0 } else { 0.0 };
    let diff_h = (p.lines.len() as f32 * LINE_H + 16.0).clamp(64.0, (screen.height() * 0.42).max(120.0));
    let h = 18.0 + 48.0 + 12.0 + reason.size().y + 12.0 + note_h + diff_h + 14.0 + 42.0 + 18.0;
    let top = (screen.max.y - 24.0 - h).max(screen.min.y + 72.0);
    let rect = Rect::from_min_size(pos2(left, top), vec2(w, h));
    let title = title(core, &name, &p.target);
    let mut decision = None;
    egui::Area::new(Id::new("config-card")).fade_in(false).fixed_pos(rect.min).order(Order::Foreground).show(ui.ctx(), |ui| {
        ui.allocate_exact_size(rect.size(), Sense::hover());
        let painter = ui.painter().clone();
        let shadow = egui::epaint::Shadow { offset: [0, 18], blur: 44, spread: 0, color: Color32::from_black_alpha(140) };
        painter.add(shadow.as_shape(rect, CornerRadius::same(18)));
        painter.rect(rect, CornerRadius::same(18), c(theme::PANEL_2), Stroke::new(1.0, ca(accent, 170)), StrokeKind::Inside);

        let face = Rect::from_min_size(rect.min + vec2(20.0, 18.0), vec2(48.0, 48.0));
        painter.rect_filled(face, CornerRadius::same(12), c([0x1A, 0x1F, 0x2A]));
        if let Some(a) = core.world.agent(&p.agent) {
            portrait(&painter, face, core, a);
        }
        let x = face.max.x + 14.0;
        let text_w = rect.max.x - 20.0 - x;
        painter.text(
            pos2(x, face.min.y + 3.0),
            Align2::LEFT_TOP,
            fit(&painter, &title, theme::ui_bold(16.5), text_w),
            theme::ui_bold(16.5),
            c(theme::TEXT),
        );
        let mut sub = format!("CONFIGURATION CHANGE  \u{00B7}  {}", p.path);
        if more > 0 {
            sub.push_str(&format!("  \u{00B7}  {more} MORE WAITING"));
        }
        painter.text(
            pos2(x, face.min.y + 29.0),
            Align2::LEFT_TOP,
            fit(&painter, &sub, theme::mono(11.0), text_w),
            theme::mono(11.0),
            c(accent),
        );

        let left = rect.min.x + 20.0;
        let mut y = face.max.y + 12.0;
        let reason_h = reason.size().y;
        painter.galley(pos2(left, y), reason, c(BODY));
        y += reason_h + 12.0;
        if warn {
            let bar = Rect::from_min_size(pos2(left, y + 2.0), vec2(3.0, 20.0));
            painter.rect_filled(bar, CornerRadius::same(1), c(WARN));
            let note = "Highlighted lines change what an agent may do or how it runs. Read them before you apply.";
            painter.text(
                pos2(left + 12.0, y + 12.0),
                Align2::LEFT_CENTER,
                fit(&painter, note, theme::ui(13.5), inner - 12.0),
                theme::ui(13.5),
                c(WARN),
            );
            y += note_h;
        }
        let diff = Rect::from_min_size(pos2(left, y), vec2(inner, diff_h));
        painter.rect(diff, CornerRadius::same(10), c(theme::TERMINAL_BG), Stroke::new(1.0, c(theme::BORDER)), StrokeKind::Inside);
        let mut list = ui.new_child(egui::UiBuilder::new().max_rect(diff.shrink(8.0)).layout(egui::Layout::top_down(egui::Align::Min)));
        egui::ScrollArea::vertical().id_salt(("config-diff", &p.id)).auto_shrink([false, false]).show(&mut list, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            let row_w = ui.available_width();
            for line in &p.lines {
                let (row, _) = ui.allocate_exact_size(vec2(row_w, LINE_H), Sense::hover());
                draw_line(ui.painter(), row, line);
            }
        });

        let y = diff.max.y + 14.0;
        let discard = Rect::from_min_size(pos2(rect.max.x - 20.0 - 112.0, y), vec2(112.0, 42.0));
        let apply = Rect::from_min_size(pos2(discard.min.x - 10.0 - 132.0, y), vec2(132.0, 42.0));
        let note = "Nothing is written until you apply.";
        painter.text(
            pos2(left, apply.center().y),
            Align2::LEFT_CENTER,
            fit(&painter, note, theme::ui(13.0), apply.min.x - left - 12.0),
            theme::ui(13.0),
            c(theme::TEXT_2),
        );
        if button(ui, apply, "Apply", ButtonStyle::Primary).clicked() {
            decision = Some(true);
        }
        if button(ui, discard, "Discard", ButtonStyle::Quiet).clicked() {
            decision = Some(false);
        }
    });
    if let Some(apply) = decision {
        decide(core, &p.id, apply);
    }
}

/// One line of the diff, tinted by its kind, in the warning colour when it
/// touches what an agent may do.
fn draw_line(painter: &Painter, row: Rect, line: &DiffLine) {
    let (mut fill, mut color, sign, font) = match line.kind {
        Kind::File => (None, theme::TEXT, "", theme::mono_medium(12.5)),
        Kind::Hunk => (None, DIM, "", theme::mono(11.5)),
        Kind::Context => (None, BODY, " ", theme::mono(12.5)),
        Kind::Add => (Some(ca(ADD, 28)), ADD, "+", theme::mono(12.5)),
        Kind::Del => (Some(ca(DEL, 28)), DEL, "-", theme::mono(12.5)),
    };
    if line.warn {
        fill = Some(ca(WARN, 48));
        color = WARN;
    }
    if let Some(fill) = fill {
        painter.rect_filled(row, CornerRadius::ZERO, fill);
    }
    if line.warn {
        painter.rect_filled(Rect::from_min_size(row.min, vec2(3.0, row.height())), CornerRadius::ZERO, c(WARN));
    }
    painter.text(pos2(row.min.x + 10.0, row.center().y), Align2::LEFT_CENTER, format!("{sign}{}", line.text), font, c(color));
}

/// A wrapped paragraph of at most `rows` rows, cut with an ellipsis.
fn paragraph(painter: &Painter, text: &str, font: FontId, color: Color32, width: f32, rows: usize) -> Arc<Galley> {
    let mut job = LayoutJob::simple(text.to_string(), font, color, width);
    job.wrap.max_rows = rows;
    job.wrap.break_anywhere = false;
    job.wrap.overflow_character = Some('\u{2026}');
    painter.layout_job(job)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const DIFF: &str = "--- a/agents/claude/agent.toml\n+++ b/agents/claude/agent.toml\n@@ -1,4 +1,7 @@\n name = \"Claude\"\n-home = \"bookshelf\"\n+home = \"desk\"\n+workspace = [\n+  \"~/code/site\",\n+]\n [runtime]\n kind = \"claude-cli\"\n@@ -9,3 +13,3 @@ [capabilities]\n system = false\n-granted = []\n+granted = [\"web\"]\n # notes\n";

    fn proposal_event(id: &str, diff: &str) -> Event {
        Event::ConfigProposal {
            id: id.into(),
            agent: "rivet".into(),
            target: "agent:claude".into(),
            path: "agents/claude/agent.toml".into(),
            reason: "You asked Claude to read the web.".into(),
            diff: diff.into(),
        }
    }

    #[test]
    fn capability_lines_are_highlighted() {
        let lines = parse_diff(DIFF);
        let warned: Vec<(Kind, &str)> = lines.iter().filter(|l| l.warn).map(|l| (l.kind, l.text.as_str())).collect();
        assert_eq!(
            warned,
            [
                (Kind::Add, "workspace = ["),
                (Kind::Add, "  \"~/code/site\","),
                (Kind::Add, "]"),
                (Kind::Del, "granted = []"),
                (Kind::Add, "granted = [\"web\"]")
            ]
        );
        assert_eq!(lines[0], DiffLine { kind: Kind::File, text: "agents/claude/agent.toml".into(), warn: false });
        assert!(
            lines.iter().any(|l| l.kind == Kind::Del && l.text == "home = \"bookshelf\"" && !l.warn),
            "a plain change is not a warning"
        );
        assert!(lines.iter().any(|l| l.kind == Kind::Context && l.text == "system = false" && !l.warn), "context is never a warning");

        // A changed runtime, a sandbox switch and a new file.
        let more = "--- a/meerora.toml\n+++ b/meerora.toml\n@@ -2,2 +2,4 @@ [sandbox]\n # comment\n-enabled = true\n+enabled = false\n+[commands]\n+key = \"Ctrl+Space\"\n--- /dev/null\n+++ b/agents/x/figure.js\n@@ -0,0 +1,1 @@\n+granted = 1\n";
        let lines = parse_diff(more);
        let warned: Vec<&str> = lines.iter().filter(|l| l.warn).map(|l| l.text.as_str()).collect();
        assert_eq!(warned, ["enabled = true", "enabled = false"], "only TOML lines, and [commands] is plain");
        assert!(lines.iter().any(|l| l.kind == Kind::File && l.text == "agents/x/figure.js  (new file)"));
    }

    #[test]
    fn proposals_follow_the_daemon() {
        let mut state = ConfigCards::default();
        apply(&mut state, &proposal_event("config-1", DIFF));
        apply(&mut state, &proposal_event("config-1", DIFF));
        apply(&mut state, &proposal_event("config-2", DIFF));
        assert_eq!(state.open.len(), 2, "a proposal sent again is kept once");
        apply(&mut state, &Event::ConfigResult { id: "config-1".into(), applied: true, message: None });
        assert_eq!(state.open.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(), ["config-2"]);
        apply(&mut state, &Event::Hello { version: 3, daemon: "d".into() });
        assert!(state.open.is_empty(), "the greeting sends the open proposals again");
    }

    /// The real UI headless: the card draws the diff with the capability
    /// lines in the warning colour, and Apply closes it.
    #[test]
    fn the_card_draws_the_diff_headless() {
        let gpu = match meerora_scene::Gpu::headless() {
            Ok(gpu) => gpu,
            Err(e) => {
                eprintln!("note: skipping, no GPU adapter ({e:#})");
                return;
            }
        };
        let (device, queue) = (&gpu.device, &gpu.queue);
        let assets = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../assets"));
        let mut core =
            Core::new(device, meerora_scene::IMAGE_FORMAT, crate::assets::Assets::without_watcher(assets), None, Arc::new(|| {}));
        core.load(device, queue);
        core.apply(&proposal_event("config-1", DIFF));
        core.ui.daemon_connected = true;
        assert!(!is_helper(&core, "rivet"), "the demo Rivet is a mock, not the helper");

        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        let screen = Rect::from_min_size(egui::Pos2::ZERO, vec2(1440.0, 900.0));
        let mut time = 0.0;
        let mut frame = |core: &mut Core, events: Vec<egui::Event>| {
            time += 0.05;
            let raw = egui::RawInput { screen_rect: Some(screen), time: Some(time), events, ..Default::default() };
            let mut output = ctx.run_ui(raw, |ui| core.build_ui(ui));
            output.textures_delta.clear();
            output
                .shapes
                .into_iter()
                .filter_map(|s| match s.shape {
                    egui::Shape::Text(t) => {
                        let color = t.galley.job.sections.first().map_or(Color32::TRANSPARENT, |s| s.format.color);
                        Some((t.galley.text().to_string(), color, t.galley.rect.translate(t.pos.to_vec2())))
                    }
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        frame(&mut core, Vec::new());
        let texts = frame(&mut core, Vec::new());
        let find = |needle: &str| texts.iter().find(|(t, _, _)| t == needle).unwrap_or_else(|| panic!("no text {needle:?}"));
        assert_eq!(find("+granted = [\"web\"]").1, c(WARN));
        assert_eq!(find("+home = \"desk\"").1, c(ADD));
        assert_eq!(find("-home = \"bookshelf\"").1, c(DEL));
        assert!(texts.iter().any(|(t, _, _)| t.starts_with("Rivet proposes a change to the settings of Claude")), "{texts:?}");
        assert!(texts.iter().any(|(t, _, _)| t.starts_with("Highlighted lines")));
        assert!(!texts.iter().any(|(t, _, _)| t.contains("Always")), "no always for configuration changes");

        let at = find("Apply").2.center();
        let press = |pressed| egui::Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        frame(&mut core, vec![egui::Event::PointerMoved(at)]);
        frame(&mut core, vec![press(true)]);
        frame(&mut core, vec![press(false)]);
        assert!(core.ui.config_changes.open.is_empty(), "Apply closes the card");
        core.apply(&Event::ConfigResult { id: "config-1".into(), applied: false, message: Some("meerora.toml changed".into()) });
        assert!(core.ui.toasts.iter().any(|t| t.error && t.text == "meerora.toml changed"), "a failed Apply shows");
    }
}
