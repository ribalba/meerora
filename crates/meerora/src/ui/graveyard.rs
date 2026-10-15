//! The graveyard in the app: the full-screen view with a card for every
//! retired agent, the confirmation cards, the "Graveyard (N)" row of the
//! sidebar and the label of a hovered tombstone. No screen is designed for
//! this yet (Q14), so the view follows the index view and screen 3.
//!
//! An agent goes to the graveyard from "Move to graveyard" at the bottom of
//! the builder in edit mode or from the context menu of its sidebar row.
//! Both ask first. The daemon then stops it, takes it out of the space and
//! moves its directory to `graveyard/`. Bring back moves it home again,
//! Delete for good moves the folder to the system trash after a second
//! question that names the folder.
//!
//! Text shown here never cites decision numbers, the user does not know them.

use std::time::Duration;

use egui::text::LayoutJob;
use egui::{
    Align2, Color32, CornerRadius, Id, Key, LayerId, Order, Painter, Pos2, Rect, Response, Sense, Stroke, StrokeKind, Ui, pos2, vec2,
};
use meerora_geom::{Vec2 as GVec2, Vec3};
use meerora_protocol::{Command, Event, GraveyardAction, RetiredAgent};

use super::{ButtonStyle, View, button, fit, logo};
use crate::engine::Core;
use crate::graveyard::{Plot, portrait_key};
use crate::theme::{self, c, ca};

/// The colour of errors and of actions that cannot be undone from here.
const DANGER: [u8; 3] = [0xE0, 0x67, 0x5A];
const CARD_W: f32 = 440.0;
const CARD_H: f32 = 196.0;
const GAP: f32 = 16.0;

/// A question that waits for the user.
#[derive(Clone, Debug, PartialEq)]
pub enum Confirm {
    /// Move an agent of the space to the graveyard.
    Retire { agent: String },
    /// Move a retired agent's folder to the trash.
    Delete { grave: String },
}

/// A command sent to the daemon whose answer has not come yet.
#[derive(Clone, Debug, PartialEq)]
struct Pending {
    action: GraveyardAction,
    /// The id the command named, an agent id for a retire, else a graveyard id.
    id: String,
    name: String,
}

#[derive(Default)]
pub struct GraveyardUi {
    pub confirm: Option<Confirm>,
    pending: Vec<Pending>,
    /// The card to show first, after a click on its tombstone.
    pub focus: Option<String>,
    /// Scroll to the focused card in the next frame.
    scroll: bool,
    /// The last refusal of the daemon, above the cards.
    pub error: Option<String>,
}

impl GraveyardUi {
    fn busy(&self, action: GraveyardAction, id: &str) -> bool {
        self.pending.iter().any(|p| p.action == action && p.id == id)
    }
}

/// Asks whether the agent `id` should move to the graveyard.
pub fn ask_retire(core: &mut Core, id: &str) {
    core.ui.graveyard.confirm = Some(Confirm::Retire { agent: id.to_string() });
}

/// Opens the graveyard view, with the card of `focus` first when given.
pub fn open(core: &mut Core, focus: Option<String>) {
    core.ui.graveyard.scroll = focus.is_some();
    core.ui.graveyard.focus = focus;
    core.ui.graveyard.error = None;
    core.ui.view = View::Graveyard;
}

/// Sends a graveyard command. Returns false when the daemon is away.
fn send(core: &mut Core, action: GraveyardAction, id: &str, name: &str) -> bool {
    let command = match action {
        GraveyardAction::Retire => Command::Retire { agent: id.into() },
        GraveyardAction::Restore => Command::Restore { agent: id.into() },
        GraveyardAction::Delete => Command::DeleteRetired { agent: id.into() },
    };
    match &core.link {
        Some(link) if core.ui.daemon_connected => {
            link.send(&command);
            core.ui.graveyard.pending.push(Pending { action, id: id.into(), name: name.into() });
            true
        }
        _ => {
            core.ui.toast(core.world.time, "The agent daemon is not running, so nothing moved.", true);
            false
        }
    }
}

/// Takes the graveyard events of the daemon. Returns true for the events
/// this module owns.
pub fn on_event(core: &mut Core, event: &Event) -> bool {
    if matches!(event, Event::Hello { .. }) {
        // A new daemon answers nothing an old one was asked.
        core.ui.graveyard.pending.clear();
    }
    if core.graveyard.apply(event) {
        return true;
    }
    let Event::GraveyardResult { action, agent, error, .. } = event else { return false };
    let pending = core.ui.graveyard.pending.iter().position(|p| p.action == *action && p.id == *agent);
    let name = pending.map(|i| core.ui.graveyard.pending.remove(i).name).unwrap_or_else(|| agent.clone());
    let now = core.world.time;
    if let Some(error) = error {
        core.ui.toast(now, error.clone(), true);
        if core.ui.view == View::Graveyard {
            core.ui.graveyard.error = Some(error.clone());
        }
        return true;
    }
    core.ui.graveyard.error = None;
    let text = match action {
        GraveyardAction::Retire if core.world.agent(agent).is_some() => {
            format!("{name} walks to the graveyard. You can bring it back from there.")
        }
        GraveyardAction::Retire => format!("{name} is in the graveyard. You can bring it back from there."),
        GraveyardAction::Restore => format!("{name} is back at its home station"),
        GraveyardAction::Delete => format!("{name} is in the trash"),
    };
    core.ui.toast(now, text, false);
    // The list follows the answer. When the last one came back, there is nothing left to see here.
    if *action == GraveyardAction::Restore && core.ui.view == View::Graveyard && core.graveyard.agents.iter().all(|a| a.id == *agent) {
        core.ui.view = View::Space;
    }
    true
}

/// "14 Oct 2026" for unix milliseconds, in local time.
pub fn day(ms: u64) -> String {
    jiff::Timestamp::from_millisecond(ms as i64)
        .map(|t| t.to_zoned(jiff::tz::TimeZone::system()).strftime("%-d %b %Y").to_string())
        .unwrap_or_default()
}

fn runs_text(runs: usize) -> String {
    match runs {
        0 => "no runs".into(),
        1 => "1 run".into(),
        n => format!("{n} runs"),
    }
}

/// The colour of an agent from its `#RRGGBB`, a light grey when it has none.
fn color_of(agent: &RetiredAgent) -> [u8; 3] {
    theme::hex_rgb(&agent.color).unwrap_or([0xC9, 0xCE, 0xD9])
}

/// A retired agent's portrait, greyed a little, or a disc in its colour.
fn portrait(painter: &Painter, core: &Core, agent: &RetiredAgent, rect: Rect) {
    painter.rect_filled(rect, CornerRadius::same(14), c([0x1A, 0x1F, 0x2A]));
    match core.portraits.get(&portrait_key(&agent.id)) {
        Some(tex) => {
            let shape = egui::epaint::RectShape::filled(rect, CornerRadius::same(14), Color32::from_gray(196))
                .with_texture(tex, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)));
            painter.add(shape);
        }
        None => {
            painter.circle_filled(rect.center(), rect.width().min(rect.height()) * 0.28, ca(color_of(agent), 200));
        }
    }
    // The band of its tombstone.
    let band = Rect::from_min_max(pos2(rect.min.x + 14.0, rect.max.y - 9.0), pos2(rect.max.x - 14.0, rect.max.y - 5.0));
    painter.rect_filled(band, CornerRadius::same(2), c(color_of(agent)));
}

/// A button for what cannot be undone here: red text, and a red fill when `filled`.
fn danger_button(ui: &mut Ui, rect: Rect, text: &str, filled: bool) -> Response {
    let resp = ui.interact(rect, ui.id().with(("danger", text, rect.min.x as i32, rect.min.y as i32)), Sense::click());
    let hovered = resp.hovered();
    let (fill, stroke, color) = if filled {
        (if hovered { c([0xEA, 0x7E, 0x72]) } else { c(DANGER) }, Stroke::NONE, c([0x26, 0x0E, 0x0B]))
    } else {
        (
            if hovered { ca(DANGER, 26) } else { Color32::TRANSPARENT },
            Stroke::new(1.0, if hovered { ca(DANGER, 170) } else { ca(DANGER, 90) }),
            c([0xF0, 0x9A, 0x90]),
        )
    };
    let painter = ui.painter();
    painter.rect(rect, CornerRadius::same(10), fill, stroke, StrokeKind::Inside);
    let font = if filled { theme::ui_bold(14.5) } else { theme::ui(14.5) };
    painter.text(rect.center(), Align2::CENTER_CENTER, text, font, color);
    if hovered {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp
}

/// A greyed button for an action that waits for the daemon.
fn waiting(ui: &Ui, rect: Rect, text: &str) {
    let painter = ui.painter();
    painter.rect(rect, CornerRadius::same(10), c([0x1A, 0x1E, 0x28]), Stroke::NONE, StrokeKind::Inside);
    painter.text(rect.center(), Align2::CENTER_CENTER, text, theme::ui(14.5), c([0x7B, 0x81, 0x90]));
}

/// A small tombstone drawn in 2D, for the sidebar row and the empty view.
fn glyph(painter: &Painter, center: Pos2, size: f32, color: Color32) {
    let w = size * 0.6;
    let h = size * 0.9;
    let body = Rect::from_center_size(center + vec2(0.0, size * 0.06), vec2(w, h));
    let r = (w / 2.0).round() as u8;
    painter.rect_stroke(body, CornerRadius { nw: r, ne: r, sw: 0, se: 0 }, Stroke::new(1.3, color), StrokeKind::Middle);
    // The ground it stands on, wider than the stone.
    painter.line_segment([pos2(body.min.x - size * 0.2, body.max.y), pos2(body.max.x + size * 0.2, body.max.y)], Stroke::new(1.3, color));
}

/// The "Graveyard (N)" row of the sidebar. Returns true when clicked.
pub fn sidebar_row(ui: &mut Ui, row: Rect, n: usize) -> bool {
    let r = ui.interact(row, Id::new("graveyard-row"), Sense::click());
    let painter = ui.painter();
    let fill = if r.hovered() { c([0x1A, 0x1F, 0x2A]) } else { c([0x14, 0x18, 0x20]) };
    let stroke = Stroke::new(1.0, if r.hovered() { c([0x3A, 0x42, 0x52]) } else { c([0x24, 0x29, 0x34]) });
    painter.rect(row, CornerRadius::same(10), fill, stroke, StrokeKind::Inside);
    let text = format!("Graveyard ({n})");
    let w = painter.layout_no_wrap(text.clone(), theme::ui(14.0), c(theme::TEXT_2)).size().x;
    glyph(painter, pos2(row.center().x - w / 2.0 - 6.0, row.center().y), 14.0, c(theme::TEXT_2));
    painter.text(pos2(row.center().x + 8.0, row.center().y), Align2::CENTER_CENTER, text, theme::ui(14.0), c([0xB9, 0xBF, 0xCC]));
    if r.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    r.clicked()
}

/// The tombstone under the pointer when it is nearer than the agent and the
/// device that the space view picked, which are indices into theirs lists.
pub fn pick(
    core: &Core,
    camera: &meerora_scene::Camera,
    origin: Vec3,
    dir: Vec3,
    agent: Option<usize>,
    device: Option<usize>,
) -> Option<String> {
    let (id, t) = core.graveyard.pick(origin, dir)?;
    let agent_t = agent.and_then(|i| core.world.bounds(&core.world.agents[i]).ray_hit(origin, dir));
    let device_t = device.and_then(|_| core.devices.pick(&core.world, camera.toward_viewer(), origin, dir)).map(|(_, t)| t);
    (agent_t.is_none_or(|a| t < a) && device_t.is_none_or(|d| t < d)).then_some(id)
}

/// "<name>, retired <day>" above the hovered tombstone of the space view.
pub fn draw_hover(ui: &Ui, core: &Core, camera: &meerora_scene::Camera, viewport: GVec2) {
    let Some(id) = core.graveyard.hover.as_deref() else { return };
    let (Some(agent), Some(top)) = (core.graveyard.agent(id), core.graveyard.top(id)) else { return };
    let Some(at) = camera.project(top + Vec3::Y * 4.0, viewport) else { return };
    let painter = ui.ctx().layer_painter(LayerId::new(Order::Background, Id::new("graveyard-hover")));
    let text = format!("{}, retired {}", agent.name, day(agent.retired_at));
    let galley = painter.layout_no_wrap(text, theme::ui_semibold(13.0), c(theme::TEXT));
    let rect = Rect::from_min_size(pos2(at.x - galley.size().x / 2.0 - 10.0, at.y - 30.0), vec2(galley.size().x + 20.0, 24.0));
    painter.rect(rect, CornerRadius::same(7), ca([0x12, 0x16, 0x1E], 236), Stroke::new(1.0, ca(color_of(agent), 150)), StrokeKind::Inside);
    painter.galley(rect.min + vec2(10.0, 12.0 - galley.size().y / 2.0), galley, c(theme::TEXT));
}

/// The confirmation card over everything else. Drawn before the view, so
/// Esc closes the question and nothing behind it.
pub fn draw_confirm(ui: &mut Ui, core: &mut Core) {
    let Some(confirm) = core.ui.graveyard.confirm.clone() else { return };
    // What the card says, and who it shows.
    let (title, body, yes, portrait_of) = match &confirm {
        Confirm::Retire { agent } => {
            let Some(name) = core.world.agent(agent).map(|a| a.name.clone()) else {
                core.ui.graveyard.confirm = None;
                return;
            };
            (
                format!("Move {name} to the graveyard?"),
                "It stops working and leaves the space. Its files, memory and history stay, and you can bring it back.".to_string(),
                "Move to graveyard",
                None,
            )
        }
        Confirm::Delete { grave } => {
            let Some(a) = core.graveyard.agent(grave).cloned() else {
                core.ui.graveyard.confirm = None;
                return;
            };
            (
                format!("Delete {} for good?", a.name),
                "meerora moves this folder to the trash, with its files, memory and history. It cannot come back from the graveyard after that."
                    .to_string(),
                "Move to trash",
                Some(a),
            )
        }
    };
    let screen = ui.ctx().content_rect();
    let w = 560.0_f32.min(screen.width() - 48.0);
    let text_w = w - 24.0 - 64.0 - 18.0 - 24.0;
    let mut job = LayoutJob::simple(body, theme::ui(14.5), c([0xC9, 0xCE, 0xD9]), text_w);
    job.wrap.break_anywhere = false;
    let galley = ui.painter().layout_job(job);
    // The folder of a delete gets a line of its own.
    let folder_h = if portrait_of.is_some() { 36.0 } else { 0.0 };
    let h = (24.0 + 26.0 + 10.0 + galley.size().y + folder_h + 24.0 + 44.0 + 22.0).max(170.0);
    let rect = Rect::from_center_size(screen.center(), vec2(w, h));
    let mut answer = None;
    egui::Area::new(Id::new("graveyard-confirm")).fade_in(false).fixed_pos(screen.min).order(Order::Foreground).show(ui.ctx(), |ui| {
        // The whole screen takes the pointer, so nothing behind the card reacts.
        let (_, backdrop) = ui.allocate_exact_size(screen.size(), Sense::click_and_drag());
        let painter = ui.painter();
        painter.rect_filled(screen, CornerRadius::ZERO, Color32::from_black_alpha(150));
        let shadow = egui::epaint::Shadow { offset: [0, 18], blur: 44, spread: 0, color: Color32::from_black_alpha(140) };
        painter.add(shadow.as_shape(rect, CornerRadius::same(18)));
        painter.rect(rect, CornerRadius::same(18), c(theme::PANEL_2), Stroke::new(1.0, ca(DANGER, 120)), StrokeKind::Inside);
        let face = Rect::from_min_size(rect.min + vec2(24.0, 24.0), vec2(64.0, 64.0));
        match (&confirm, &portrait_of) {
            (Confirm::Retire { agent }, _) => {
                if let Some(a) = core.world.agent(agent) {
                    painter.rect_filled(face, CornerRadius::same(14), c([0x1A, 0x1F, 0x2A]));
                    super::portrait(painter, face, core, a);
                }
            }
            (_, Some(a)) => portrait(painter, core, a, face),
            _ => {}
        }
        let x = face.max.x + 18.0;
        let title = fit(painter, &title, theme::ui_bold(18.0), rect.max.x - 24.0 - x);
        painter.text(pos2(x, rect.min.y + 26.0), Align2::LEFT_TOP, title, theme::ui_bold(18.0), c(theme::TEXT));
        let text_y = rect.min.y + 26.0 + 26.0 + 10.0;
        let text_h = galley.size().y;
        painter.galley(pos2(x, text_y), galley, c([0xC9, 0xCE, 0xD9]));
        if let Some(a) = &portrait_of {
            let chip = Rect::from_min_size(pos2(x, text_y + text_h + 10.0), vec2(rect.max.x - 24.0 - x, 26.0));
            painter.rect_filled(chip, CornerRadius::same(7), c([0x1B, 0x20, 0x2B]));
            let shown = folder_text(painter, &a.path, chip.width() - 20.0);
            painter.text(pos2(chip.min.x + 10.0, chip.center().y), Align2::LEFT_CENTER, shown, theme::mono(11.5), c(theme::TEXT));
        }
        let cancel = Rect::from_min_size(pos2(rect.max.x - 24.0 - 104.0, rect.max.y - 22.0 - 44.0), vec2(104.0, 44.0));
        let yes_w = painter.layout_no_wrap(yes.to_string(), theme::ui_bold(14.5), Color32::WHITE).size().x + 40.0;
        let yes_r = Rect::from_min_size(pos2(cancel.min.x - 12.0 - yes_w, cancel.min.y), vec2(yes_w, 44.0));
        if danger_button(ui, yes_r, yes, true).clicked() {
            answer = Some(true);
        }
        if button(ui, cancel, "Cancel", ButtonStyle::Quiet).clicked() {
            answer = Some(false);
        }
        // A click beside the card keeps the question open, it is only a question.
        let _ = backdrop;
    });
    // Above the talk field and the other foreground areas of the views.
    ui.ctx().move_to_top(LayerId::new(Order::Foreground, Id::new("graveyard-confirm")));
    if ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Escape)) {
        answer = Some(false);
    }
    let Some(yes) = answer else { return };
    core.ui.graveyard.confirm = None;
    if !yes {
        return;
    }
    match confirm {
        Confirm::Retire { agent } => {
            let name = core.world.agent(&agent).map_or(agent.clone(), |a| a.name.clone());
            let editing = core.ui.builder.as_ref().is_some_and(|b| b.editing.as_deref() == Some(agent.as_str()));
            if send(core, GraveyardAction::Retire, &agent, &name) && (editing || core.ui.view == View::Agent(agent.clone())) {
                // Back to the space, where the agent walks to the graveyard before it leaves.
                core.ui.view = View::Space;
            }
        }
        Confirm::Delete { grave } => {
            let name = core.graveyard.agent(&grave).map_or(grave.clone(), |a| a.name.clone());
            send(core, GraveyardAction::Delete, &grave, &name);
        }
    }
}

/// A folder for one line of `width`: the home folder as `~`, and cut at the
/// front when it is still too long, since its end names the agent.
fn folder_text(painter: &Painter, path: &std::path::Path, width: f32) -> String {
    let full = path.display().to_string();
    let text = match std::env::var("HOME") {
        Ok(home) if !home.is_empty() && full.starts_with(&format!("{home}/")) => format!("~{}", &full[home.len()..]),
        _ => full,
    };
    let fits = |t: &str| painter.layout_no_wrap(t.to_string(), theme::mono(11.5), Color32::WHITE).size().x <= width;
    if fits(&text) {
        return text;
    }
    let chars: Vec<char> = text.chars().collect();
    (1..chars.len()).map(|k| format!("…{}", chars[k..].iter().collect::<String>())).find(|t| fits(t)).unwrap_or_default()
}

/// What a click in the view asked for.
enum Act {
    Restore(String),
    Open(std::path::PathBuf),
    Delete(String),
}

/// The graveyard view: a card for every retired agent.
pub fn draw(ui: &mut Ui, core: &mut Core) {
    let screen = ui.ctx().content_rect();
    ui.ctx().layer_painter(LayerId::background()).rect_filled(screen, CornerRadius::ZERO, c(theme::BG));
    let mut back = false;
    let mut act = None;
    egui::Area::new(Id::new("graveyard")).fade_in(false).fixed_pos(screen.min).order(Order::Middle).show(ui.ctx(), |ui| {
        ui.allocate_exact_size(screen.size(), Sense::hover());
        back = header(ui, core, screen);
        let body = Rect::from_min_max(pos2(24.0, 92.0), pos2(screen.max.x - 24.0, screen.max.y - 24.0));
        act = cards(ui, core, body);
    });
    if ui.input(|i| i.key_pressed(Key::Escape)) && core.ui.graveyard.confirm.is_none() {
        back = true;
    }
    if !core.ui.graveyard.pending.is_empty() {
        ui.ctx().request_repaint_after(Duration::from_millis(100));
    }
    match act {
        Some(Act::Restore(id)) => {
            let name = core.graveyard.agent(&id).map_or(id.clone(), |a| a.name.clone());
            send(core, GraveyardAction::Restore, &id, &name);
        }
        Some(Act::Open(path)) => {
            if let Err(e) = super::apps::files::open_external(&path) {
                core.ui.toast(core.world.time, e, true);
            }
        }
        Some(Act::Delete(id)) => core.ui.graveyard.confirm = Some(Confirm::Delete { grave: id }),
        None => {}
    }
    if back {
        core.ui.view = View::Space;
    }
}

/// Returns true for the back button.
fn header(ui: &mut Ui, core: &Core, screen: Rect) -> bool {
    let painter = ui.painter();
    super::hline(painter, 72.0, 0.0, screen.max.x);
    let back_r = Rect::from_min_size(pos2(24.0, 14.0), vec2(132.0, 44.0));
    let back = button(ui, back_r, "Your space", ButtonStyle::Quiet).clicked();
    let painter = ui.painter();
    logo(painter, pos2(back_r.max.x + 26.0, 36.0), 18.0);
    painter.text(pos2(back_r.max.x + 46.0, 26.0), Align2::LEFT_CENTER, "Graveyard", theme::ui_bold(18.0), c(theme::TEXT));
    let n = core.graveyard.agents.len();
    let sub = match n {
        1 => "1 RETIRED AGENT".to_string(),
        n => format!("{n} RETIRED AGENTS"),
    };
    painter.text(pos2(back_r.max.x + 46.0, 48.0), Align2::LEFT_CENTER, sub, theme::mono(11.0), c(theme::TEXT_2));
    back
}

/// What the garden shows of the graveyard, for the line under the intro.
fn plot_line(core: &Core) -> String {
    let n = core.graveyard.agents.len();
    let Some(space) = core.world.space.as_ref() else { return String::new() };
    if Plot::of(&space.desc).is_none() {
        return "THIS SPACE HAS NO GRAVEYARD PLOT, SO THE GARDEN SHOWS NO TOMBSTONES".into();
    }
    match n.saturating_sub(core.graveyard.stones()) {
        0 if n == 1 => "ITS TOMBSTONE STANDS IN THE GARDEN, BEHIND THE TREE".into(),
        0 => "THEIR TOMBSTONES STAND IN THE GARDEN, BEHIND THE TREE".into(),
        1 => "THE PLOT IN THE GARDEN IS FULL, SO THE NEWEST HAS NO TOMBSTONE".into(),
        missing => format!("THE PLOT IN THE GARDEN IS FULL, SO THE {missing} NEWEST HAVE NO TOMBSTONE"),
    }
}

fn cards(ui: &mut Ui, core: &mut Core, rect: Rect) -> Option<Act> {
    let painter = ui.painter();
    super::panel(painter, rect, 18);
    painter.text(rect.min + vec2(20.0, 20.0), Align2::LEFT_TOP, "Retired agents", theme::ui_bold(22.0), c(theme::TEXT));
    let intro = painter.layout(
        "Agents rest here with their files, memory and history. Bring one back and it returns to its home station. Delete for good moves its folder to the trash.".into(),
        theme::ui(14.0),
        c(theme::TEXT_2),
        rect.width() - 40.0,
    );
    let mut y = rect.min.y + 50.0 + intro.size().y + 12.0;
    painter.galley(rect.min + vec2(20.0, 50.0), intro, c(theme::TEXT_2));
    let (line, color) = match &core.ui.graveyard.error {
        Some(e) => (e.clone(), DANGER),
        None => (plot_line(core), theme::TEXT_2),
    };
    if !line.is_empty() {
        let galley = painter.layout(line, theme::mono(11.0), c(color), rect.width() - 40.0);
        let h = galley.size().y;
        painter.galley(pos2(rect.min.x + 20.0, y), galley, c(color));
        y += h + 16.0;
    }

    let agents = core.graveyard.agents.clone();
    if agents.is_empty() {
        let center = pos2(rect.center().x, (y + rect.max.y) / 2.0);
        glyph(painter, center - vec2(0.0, 34.0), 40.0, c([0x4A, 0x50, 0x5E]));
        painter.text(center + vec2(0.0, 6.0), Align2::CENTER_CENTER, "The graveyard is empty", theme::ui_semibold(17.0), c(theme::TEXT_2));
        painter.text(
            center + vec2(0.0, 32.0),
            Align2::CENTER_CENTER,
            "Agents you move here keep their files, memory and history, and you can bring them back.",
            theme::ui(14.0),
            c([0x7B, 0x81, 0x90]),
        );
        return None;
    }

    let list = Rect::from_min_max(pos2(rect.min.x + 20.0, y), pos2(rect.max.x - 20.0, rect.max.y - 16.0));
    let columns = (((list.width() + GAP) / (CARD_W + GAP)).floor() as usize).max(1);
    let card_w = ((list.width() + GAP) / columns as f32 - GAP).min(CARD_W + 120.0);
    let mut list_ui = ui.new_child(egui::UiBuilder::new().max_rect(list).layout(egui::Layout::top_down(egui::Align::Min)));
    let mut act = None;
    let focus = core.ui.graveyard.focus.clone();
    let scroll = std::mem::take(&mut core.ui.graveyard.scroll);
    egui::ScrollArea::vertical().id_salt("graveyard-cards").auto_shrink([false, false]).show(&mut list_ui, |ui| {
        for row in agents.chunks(columns) {
            let (line, _) = ui.allocate_exact_size(vec2(list.width() - 8.0, CARD_H + GAP), Sense::hover());
            for (k, agent) in row.iter().enumerate() {
                let card = Rect::from_min_size(pos2(line.min.x + k as f32 * (card_w + GAP), line.min.y), vec2(card_w, CARD_H));
                let focused = focus.as_deref() == Some(agent.id.as_str());
                if focused && scroll {
                    ui.scroll_to_rect(card, Some(egui::Align::Center));
                }
                if let Some(a) = card_ui(ui, core, agent, card, focused) {
                    act = Some(a);
                }
            }
        }
    });
    act
}

fn card_ui(ui: &mut Ui, core: &Core, agent: &RetiredAgent, card: Rect, focused: bool) -> Option<Act> {
    let painter = ui.painter();
    let stroke = if focused { Stroke::new(1.5, ca(color_of(agent), 200)) } else { Stroke::new(1.0, c([0x22, 0x27, 0x32])) };
    painter.rect(card, CornerRadius::same(16), c([0x16, 0x1A, 0x23]), stroke, StrokeKind::Inside);
    let face = Rect::from_min_size(card.min + vec2(16.0, 16.0), vec2(104.0, 104.0));
    portrait(painter, core, agent, face);
    let x = face.max.x + 16.0;
    let right = card.max.x - 16.0;
    let name = fit(painter, &agent.name, theme::ui_semibold(17.0), right - x);
    painter.text(pos2(x, card.min.y + 16.0), Align2::LEFT_TOP, name, theme::ui_semibold(17.0), c(theme::TEXT));
    let meta = format!("RETIRED {} · {}", day(agent.retired_at).to_uppercase(), runs_text(agent.runs).to_uppercase());
    let meta = fit(painter, &meta, theme::mono(10.5), right - x);
    painter.text(pos2(x, card.min.y + 42.0), Align2::LEFT_TOP, meta, theme::mono(10.5), c(theme::TEXT_2));
    let last = match &agent.last_activity {
        Some(text) => format!("Last: {text}"),
        None => "Nothing under its name when it left.".to_string(),
    };
    let mut job = LayoutJob::simple(last, theme::ui(13.5), c([0xC9, 0xCE, 0xD9]), right - x);
    job.wrap.max_rows = 2;
    job.wrap.break_anywhere = false;
    job.wrap.overflow_character = Some('…');
    painter.galley(pos2(x, card.min.y + 62.0), painter.layout_job(job), c([0xC9, 0xCE, 0xD9]));
    let folder = format!("{}/{}", meerora_protocol::GRAVEYARD_DIR, agent.id);
    if agent.agent != agent.id {
        let was = fit(painter, &format!("{folder} · was {}", agent.agent), theme::mono(10.5), right - x);
        painter.text(pos2(x, face.max.y - 12.0), Align2::LEFT_CENTER, was, theme::mono(10.5), c([0x7B, 0x81, 0x90]));
    } else {
        painter.text(pos2(x, face.max.y - 12.0), Align2::LEFT_CENTER, folder, theme::mono(10.5), c([0x7B, 0x81, 0x90]));
    }

    let y = card.max.y - 16.0 - 40.0;
    let restore = Rect::from_min_size(pos2(card.min.x + 16.0, y), vec2(118.0, 40.0));
    let open = Rect::from_min_size(pos2(restore.max.x + 8.0, y), vec2(116.0, 40.0));
    let delete = Rect::from_min_size(pos2(right - 138.0, y), vec2(138.0, 40.0));
    let state = &core.ui.graveyard;
    let mut act = None;
    if state.busy(GraveyardAction::Restore, &agent.id) {
        waiting(ui, restore, "Bringing back…");
    } else if button(ui, restore, "Bring back", ButtonStyle::Primary).clicked() {
        act = Some(Act::Restore(agent.id.clone()));
    }
    if button(ui, open, "Open folder", ButtonStyle::Quiet).clicked() {
        act = Some(Act::Open(agent.path.clone()));
    }
    if state.busy(GraveyardAction::Delete, &agent.id) {
        waiting(ui, delete, "Moving to trash…");
    } else if danger_button(ui, delete, "Delete for good", false).clicked() {
        act = Some(Act::Delete(agent.id.clone()));
    }
    act
}

/// The label for the context menu and the builder.
pub const MOVE: &str = "Move to graveyard";

/// The quiet danger button at the bottom of the builder in edit mode.
/// Returns true when clicked.
pub fn builder_button(ui: &mut Ui, rect: Rect) -> bool {
    let w = ui.painter().layout_no_wrap(MOVE.to_string(), theme::ui(14.5), Color32::WHITE).size().x + 40.0;
    let r = Rect::from_min_size(rect.min, vec2(w, rect.height()));
    let clicked = danger_button(ui, r, MOVE, false).clicked();
    let note = "It stops working and leaves the space. Its files, memory and history stay.";
    let painter = ui.painter();
    let note = fit(painter, note, theme::ui(13.0), rect.max.x - r.max.x - 16.0);
    painter.text(pos2(r.max.x + 14.0, r.center().y), Align2::LEFT_CENTER, note, theme::ui(13.0), c([0x7B, 0x81, 0x90]));
    clicked
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::Arc;

    #[test]
    fn days_and_runs_read_plainly() {
        assert_eq!(runs_text(0), "no runs");
        assert_eq!(runs_text(1), "1 run");
        assert_eq!(runs_text(12), "12 runs");
        let d = day(1_792_000_000_000);
        assert!(d.ends_with(" 2026") && d.split(' ').count() == 3, "{d}");
    }

    fn retired(assets: &std::path::Path, id: &str, name: &str, color: &str, at: u64) -> RetiredAgent {
        RetiredAgent {
            id: id.into(),
            agent: id.into(),
            name: name.into(),
            color: color.into(),
            retired_at: at,
            figure: std::fs::read_to_string(assets.join("agents").join(id).join("figure.js")).unwrap(),
            last_activity: Some("batch 3 of 5".into()),
            runs: 4,
            path: PathBuf::from("/nowhere/graveyard").join(id),
        }
    }

    /// The real space with two tombstones, driven through egui like a user:
    /// the pointer on a stone hovers it, a click opens the graveyard at its
    /// card, a drag still turns the view, and the view and its questions draw.
    #[test]
    fn tombstones_pick_like_devices_and_the_view_draws_headless() {
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
            Core::new(device, meerora_scene::IMAGE_FORMAT, crate::assets::Assets::without_watcher(assets.clone()), None, Arc::new(|| {}));
        core.load(device, queue);
        let agents = vec![retired(&assets, "ember", "Ember", "#C7784A", 2), retired(&assets, "quill", "Quill", "#EFE8DA", 1)];
        core.apply(&Event::Graveyard { agents });
        core.step(device, queue, 1.0 / 30.0);
        assert_eq!(core.graveyard.stones(), 2);
        assert!(core.portraits.get(&portrait_key("ember")).is_some(), "the card has a portrait");

        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        let screen = Rect::from_min_size(Pos2::ZERO, vec2(1440.0, 900.0));
        let viewport = GVec2::new(screen.width(), screen.height());
        let top = core.graveyard.top("ember").unwrap();
        let p = core.scene_camera(viewport).project(top - Vec3::Y * 14.0, viewport).expect("the plot is in view");
        let at = pos2(p.x, p.y);
        let mut time = 0.0;
        let mut frame = |core: &mut Core, events: Vec<egui::Event>| {
            time += 0.05;
            let raw = egui::RawInput { screen_rect: Some(screen), time: Some(time), events, ..Default::default() };
            let mut output = ctx.run_ui(raw, |ui| core.build_ui(ui));
            output.textures_delta.clear();
        };
        let button = |pos: Pos2, pressed: bool| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };

        frame(&mut core, vec![egui::Event::PointerMoved(at)]);
        frame(&mut core, vec![egui::Event::PointerMoved(at)]);
        assert_eq!(core.graveyard.hover.as_deref(), Some("ember"));

        // A drag that starts on a stone turns the view and opens nothing.
        frame(&mut core, vec![button(at, true)]);
        frame(&mut core, vec![egui::Event::PointerMoved(at + vec2(60.0, 0.0))]);
        frame(&mut core, vec![egui::Event::PointerMoved(at + vec2(120.0, 0.0))]);
        frame(&mut core, vec![button(at + vec2(120.0, 0.0), false)]);
        assert_eq!(core.ui.view, View::Space);
        assert!(core.ui.view_yaw.abs() > 1.0, "the drag turned the view");

        // A click opens the graveyard at its card.
        core.ui.view_yaw = 0.0;
        frame(&mut core, vec![egui::Event::PointerMoved(at)]);
        frame(&mut core, vec![button(at, true)]);
        frame(&mut core, vec![button(at, false)]);
        assert_eq!(core.ui.view, View::Graveyard);
        assert_eq!(core.ui.graveyard.focus.as_deref(), Some("ember"));
        frame(&mut core, Vec::new());
        frame(&mut core, Vec::new());

        // The second question names the folder, and Esc closes only the question.
        core.ui.graveyard.confirm = Some(Confirm::Delete { grave: "quill".into() });
        frame(&mut core, Vec::new());
        assert!(core.ui.graveyard.confirm.is_some());
        let escape =
            egui::Event::Key { key: Key::Escape, physical_key: None, pressed: true, repeat: false, modifiers: egui::Modifiers::NONE };
        frame(&mut core, vec![escape.clone()]);
        assert_eq!((core.ui.graveyard.confirm.clone(), core.ui.view.clone()), (None, View::Graveyard));

        // Asking to retire an agent works without the daemon, and sending without it only says so.
        ask_retire(&mut core, "wren");
        frame(&mut core, Vec::new());
        assert_eq!(core.ui.graveyard.confirm, Some(Confirm::Retire { agent: "wren".into() }));
        assert!(!send(&mut core, GraveyardAction::Retire, "wren", "Wren"));
        assert!(core.ui.toasts.iter().any(|t| t.error && t.text.contains("not running")));
        frame(&mut core, vec![escape]);
        assert_eq!(core.ui.graveyard.confirm, None);

        // An agent that leaves frees its view and figure, and one that appears comes in from its directory.
        crate::ui::open_agent(&mut core, "wren");
        core.apply(&Event::AgentRemoved { agent: "wren".into() });
        assert_eq!(core.ui.view, View::Space);
        assert!(core.world.agent("wren").is_none() && core.agent_dir("wren").is_none() && core.portraits.get("wren").is_none());
        core.apply(&Event::AgentSpawned { agent: "wren".into(), name: "Wren".into(), system: false });
        core.step(device, queue, 1.0 / 30.0);
        assert!(core.world.agent("wren").is_some_and(|a| a.online && a.figure.is_some()), "back with its figure");

        // A Graveyard event without agents takes the stones away.
        core.apply(&Event::Graveyard { agents: Vec::new() });
        core.step(device, queue, 1.0 / 30.0);
        assert_eq!(core.graveyard.stones(), 0);
        assert!(core.portraits.get(&portrait_key("ember")).is_none(), "the portraits go with them");
    }

    #[test]
    fn pending_commands_mark_their_buttons_busy() {
        let mut state = GraveyardUi::default();
        state.pending.push(Pending { action: GraveyardAction::Retire, id: "wren".into(), name: "Wren".into() });
        assert!(state.busy(GraveyardAction::Retire, "wren"));
        assert!(!state.busy(GraveyardAction::Restore, "wren"));
    }
}
