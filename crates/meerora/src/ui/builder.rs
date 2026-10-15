//! The agent builder (screen 3). Step 1 describes the look to an LLM that
//! writes the figure script, with a live preview. Steps 2 and 3 set the mind,
//! access, windows and schedule. Creating the agent writes its directory
//! (D21). The same screens edit an existing agent.
//!
//! Text shown here never cites decision numbers, the user does not know them.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use egui::{Align2, CornerRadius, Id, LayerId, Order, Rect, Sense, Stroke, StrokeKind, Ui, pos2, vec2};
use meerora_protocol::{
    AgentConfig, AgentRole, App, CapabilitiesConfig, Command, DraftStatus, OnDone, Role, RuntimeConfig, RuntimeKind, ScheduleEntry,
};
use meerora_script::{FigureDesc, Limits, ParamKind};

use super::{ButtonStyle, View, button, label, logo};
use crate::engine::Core;
use crate::preview::PreviewAnim;
use crate::theme::{self, c, ca};

/// Shown until the builder wrote a first version, so the animations can be tried at once.
const STARTER: &str = r##"// A plain starter figure. Describe your agent on the left to replace it.
const body = capsule(22, 80).material("ceramic", "#9AA1B0");
const visor = box(30, 13, 8).bevel(3).material("glaze", "#20242E").at(0, 52, 17);
const eyes = [-6, 6].map((x) => sphere(2.6, 1).material("glaze", "#FFE7B0").glow(1).at(x, 55, 21));
export default figure({
  parts: [body, visor, ...eyes],
  anchors: { ring: body.top.above(14), face: visor },
  animations: { working: bob(1.5, 1.4), waiting: hop(2).then(face("viewer")), resting: breathe(4).eyes("closed") },
});
"##;

pub struct ScheduleDraft {
    pub every: String,
    pub at: String,
    pub task: String,
    pub station: String,
    pub quiet_hours: String,
}

pub struct BuilderState {
    pub session: String,
    pub step: u8,
    pub name: String,
    pub prompt: String,
    pub messages: Vec<(Role, String)>,
    pub working: Option<String>,
    /// Validated scripts, newest last. The last one is shown.
    pub versions: Vec<String>,
    pub figure: Option<FigureDesc>,
    /// Changes whenever `figure` changes, so the preview re-uploads it.
    /// Unique across builder sessions, see [`next_figure_version`].
    pub figure_version: u64,
    pub anim: PreviewAnim,
    pub anim_since: f64,
    pub params: HashMap<String, serde_json::Value>,
    pub show_script: bool,
    pub yaw: f32,
    /// Pixel size of the preview stage in the last frame.
    pub preview_px: (u32, u32),
    pub runtime: RuntimeKind,
    pub model: String,
    pub instructions: String,
    pub shell: bool,
    pub web: bool,
    pub on_done: OnDone,
    pub home: String,
    pub schedule: Vec<ScheduleDraft>,
    pub error: Option<String>,
    /// Windows the agent view opens with, besides the chat. You can open
    /// and close apps later in the agent view.
    pub windows: Vec<App>,
    /// The model is typed in instead of picked from the list.
    pub custom_model: bool,
    /// Models Ollama has installed, fetched once when needed.
    pub ollama_models: Option<Result<Vec<String>, String>>,
    /// The agent being edited, None while creating a new one.
    pub editing: Option<String>,
    /// Set when the builder makes the user's personal agent (D26).
    pub role: Option<AgentRole>,
    /// Folders the agent may read and write besides its own directory,
    /// written with `~` for the home (`workspace` in agent.toml).
    pub workspace: Vec<std::path::PathBuf>,
    /// The path typed into "Add folder".
    pub workspace_input: String,
    /// Why the typed folder cannot be a workspace folder.
    pub workspace_error: Option<String>,
}

impl BuilderState {
    pub fn new(now: f64, home: String) -> Self {
        let figure = meerora_script::build_figure(STARTER, "starter.js", &Limits::default()).ok();
        Self {
            session: format!("build-{}", (now * 1000.0) as u64),
            step: 1,
            name: String::new(),
            prompt: String::new(),
            messages: Vec::new(),
            working: None,
            versions: Vec::new(),
            figure,
            figure_version: next_figure_version(),
            anim: PreviewAnim::Working,
            anim_since: now,
            params: HashMap::new(),
            show_script: false,
            yaw: 0.0,
            preview_px: (0, 0),
            runtime: RuntimeKind::ClaudeCli,
            model: "sonnet".into(),
            instructions: String::new(),
            shell: false,
            web: false,
            on_done: OnDone::Wait,
            home,
            schedule: Vec::new(),
            error: None,
            windows: vec![App::Editor, App::Terminal, App::Browser],
            custom_model: false,
            ollama_models: None,
            editing: None,
            role: None,
            workspace: Vec::new(),
            workspace_input: String::new(),
            workspace_error: None,
        }
    }

    /// Fills the builder from an existing agent directory.
    fn from_agent(now: f64, dir: &meerora_protocol::AgentDir) -> Self {
        let c = &dir.config;
        let mut state = Self::new(now, c.home.clone());
        state.editing = Some(dir.id.clone());
        state.name = c.name.clone();
        if let Ok(script) = std::fs::read_to_string(dir.figure_path()) {
            state.accept(script);
        }
        if let Some(r) = &c.runtime {
            state.runtime = r.kind;
            state.model = r.model.clone().unwrap_or_default();
        }
        state.custom_model =
            !model_options(state.runtime, &[]).iter().any(|(v, _)| *v == state.model) && state.runtime != RuntimeKind::Ollama;
        state.instructions = c.instructions.clone().unwrap_or_default();
        state.shell = c.capabilities.has("shell");
        state.web = c.capabilities.has("web");
        state.on_done = c.on_done;
        state.windows = c.apps.iter().copied().filter(|a| *a != App::Chat).collect();
        state.workspace = c.workspace.clone();
        state.schedule = c
            .schedule
            .iter()
            .map(|e| ScheduleDraft {
                every: e.every.clone().unwrap_or_default(),
                at: e.at.clone().unwrap_or_default(),
                task: e.task.clone(),
                station: e.station.clone().unwrap_or_default(),
                quiet_hours: e.quiet_hours.clone().unwrap_or_default(),
            })
            .collect();
        state
            .messages
            .push((Role::System, format!("Editing {}. Describe a change to the look, or switch to the other steps above.", c.name)));
        state
    }

    fn script(&self) -> &str {
        self.versions.last().map_or(STARTER, String::as_str)
    }

    /// Takes a new validated script from the builder.
    pub fn accept(&mut self, script: String) {
        match meerora_script::build_figure(&script, "draft.js", &Limits::default()) {
            Ok(figure) => {
                self.figure = Some(figure);
                self.figure_version = next_figure_version();
                self.versions.push(script);
            }
            Err(e) => self.messages.push((Role::System, format!("The new version does not build here: {e}"))),
        }
    }

    fn undo(&mut self) {
        if self.versions.pop().is_some() {
            self.figure = meerora_script::build_figure(self.script(), "draft.js", &Limits::default()).ok();
            self.figure_version = next_figure_version();
        }
    }
}

/// Opens the builder on an existing agent.
/// A figure version no builder session has used before. The preview keeps
/// the last figure it uploaded, and a new session starting again at 1 would
/// show the figure of the previous one.
fn next_figure_version() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

pub fn open_edit(core: &mut Core, id: &str) {
    let Some(dir) = core.agent_dir(id).cloned() else { return };
    core.ui.builder = Some(BuilderState::from_agent(core.world.time, &dir));
    core.ui.view = View::Builder;
}

/// Model choices for a runtime as (value, label). An empty value means the
/// runtime's own default.
fn model_options(kind: RuntimeKind, ollama: &[String]) -> Vec<(String, String)> {
    let pairs = |list: &[(&str, &str)]| list.iter().map(|(v, l)| (v.to_string(), l.to_string())).collect::<Vec<_>>();
    match kind {
        RuntimeKind::ClaudeCli => pairs(&[("", "Default"), ("opus", "Opus"), ("sonnet", "Sonnet"), ("haiku", "Haiku")]),
        RuntimeKind::CodexCli => {
            let default = codex_default_model().map_or("Default".to_string(), |m| format!("Default ({m})"));
            let mut list = vec![(String::new(), default)];
            list.extend(pairs(&[("gpt-6.1-sol", "gpt-6.1-sol"), ("gpt-6-luna", "gpt-6-luna")]));
            list
        }
        RuntimeKind::Ollama => ollama.iter().map(|m| (m.clone(), m.clone())).collect(),
        _ => Vec::new(),
    }
}

/// The model set in `~/.codex/config.toml`, which codex uses when meerora names none.
fn codex_default_model() -> Option<String> {
    let home = std::env::var_os("HOME")?;
    let text = std::fs::read_to_string(std::path::Path::new(&home).join(".codex/config.toml")).ok()?;
    let value: toml::Value = toml::from_str(&text).ok()?;
    value.get("model")?.as_str().map(str::to_string)
}

/// Installed Ollama models from its HTTP API.
fn fetch_ollama_models(url: &str) -> Result<Vec<String>, String> {
    let config = ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(2))).build();
    let agent = ureq::Agent::new_with_config(config);
    let mut response = agent.get(&format!("{url}/api/tags")).call().map_err(|_| format!("Ollama is not running at {url}"))?;
    let tags: serde_json::Value = response.body_mut().read_json().map_err(|e| e.to_string())?;
    let mut names: Vec<String> =
        tags["models"].as_array().into_iter().flatten().filter_map(|m| m["name"].as_str().map(str::to_string)).collect();
    names.sort();
    Ok(names)
}

/// Applies a builder event from the daemon.
pub fn on_draft(state: &mut BuilderState, status: DraftStatus, script: Option<String>, message: String) {
    match status {
        DraftStatus::Working => state.working = Some(message),
        DraftStatus::Ready => {
            state.working = None;
            state.messages.push((Role::Agent, message));
            if let Some(script) = script {
                state.accept(script);
            }
        }
        DraftStatus::Failed => {
            state.working = None;
            state.messages.push((Role::System, message));
        }
    }
}

pub fn draw(ui: &mut Ui, core: &mut Core) {
    let Some(mut state) = core.ui.builder.take() else {
        core.ui.view = View::Space;
        return;
    };
    let screen = ui.ctx().content_rect();
    let painter = ui.ctx().layer_painter(LayerId::background());
    painter.rect_filled(screen, CornerRadius::ZERO, c(theme::BG));
    let mut close = false;
    let mut create = false;
    let mut retire = false;
    egui::Area::new(Id::new("builder")).fade_in(false).fixed_pos(screen.min).order(Order::Middle).show(ui.ctx(), |ui| {
        ui.allocate_exact_size(screen.size(), Sense::hover());
        let (c1, c2) = header(ui, core, &mut state, screen);
        close = c1;
        create = c2;
        // In edit mode the bottom row holds Move to graveyard, except for the helper, which always stays.
        let retirable = state.editing.as_deref().is_some_and(|id| !super::config_card::is_helper(core, id));
        let bottom = if retirable { screen.max.y - 24.0 - 56.0 } else { screen.max.y - 24.0 };
        let body = Rect::from_min_max(pos2(24.0, 92.0), pos2(screen.max.x - 24.0, bottom));
        match state.step {
            1 => step_look(ui, core, &mut state, body),
            2 => step_mind(ui, core, &mut state, body),
            _ => step_schedule(ui, core, &mut state, body),
        }
        if retirable {
            let row = Rect::from_min_max(pos2(24.0, bottom + 12.0), pos2(screen.max.x - 24.0, screen.max.y - 24.0));
            retire = super::graveyard::builder_button(ui, row);
        }
    });
    if retire && let Some(id) = state.editing.clone() {
        super::graveyard::ask_retire(core, &id);
    }
    if close {
        core.ui.view = match &state.editing {
            Some(id) => View::Agent(id.clone()),
            None => View::Space,
        };
        return;
    }
    if create && let Some(id) = state.editing.clone() {
        match save_agent(core, &state, &id) {
            Ok(()) => {
                core.ui.toast(core.world.time, format!("Saved {}", state.name.trim()), false);
                if let Some(link) = &core.link {
                    link.send(&Command::ReloadAgents);
                }
                core.ui.view = View::Agent(id.clone());
                core.pending_agent = Some(id);
                return;
            }
            Err(e) => state.error = Some(e),
        }
    } else if create {
        match create_agent(core, &state) {
            Ok(id) => {
                core.ui.toast(core.world.time, format!("{} moved into your space", state.name.trim()), false);
                if let Some(link) = &core.link {
                    link.send(&Command::ReloadAgents);
                }
                core.ui.view = View::Space;
                core.pending_agent = Some(id.clone());
                // The interview fills the new personal agent's profile (D26).
                if state.role == Some(AgentRole::Personal) {
                    super::profile::open_for(core, &id);
                }
                return;
            }
            Err(e) => state.error = Some(e),
        }
    }
    core.ui.builder = Some(state);
}

/// Returns (cancel, create).
fn header(ui: &mut Ui, core: &Core, state: &mut BuilderState, screen: Rect) -> (bool, bool) {
    let painter = ui.painter();
    super::hline(painter, 72.0, 0.0, screen.max.x);
    let back = Rect::from_min_size(pos2(24.0, 14.0), vec2(132.0, 44.0));
    let back_label = if state.editing.is_some() { "Back" } else { "Your space" };
    let mut cancel = button(ui, back, back_label, ButtonStyle::Quiet).clicked();
    let painter = ui.painter();
    logo(painter, pos2(back.max.x + 26.0, 36.0), 18.0);
    let title = match (&state.editing, state.role) {
        (Some(_), _) => format!("Edit {}", state.name.trim()),
        (None, Some(AgentRole::Personal)) => "Your personal agent".to_string(),
        _ => "New agent".to_string(),
    };
    painter.text(pos2(back.max.x + 46.0, 26.0), Align2::LEFT_CENTER, title, theme::ui_bold(18.0), c(theme::TEXT));
    let steps = ["LOOK", "MIND AND ACCESS", "SCHEDULE"];
    painter.text(
        pos2(back.max.x + 46.0, 48.0),
        Align2::LEFT_CENTER,
        format!("STEP {} OF 3 · {}", state.step, steps[state.step as usize - 1]),
        theme::mono(11.0),
        c(theme::TEXT_2),
    );

    let labels = ["Look", "Mind and access", "Schedule"];
    let widths = [92.0, 172.0, 118.0];
    let total: f32 = widths.iter().sum::<f32>() + 16.0;
    let mut x = screen.center().x - total / 2.0;
    for (i, (text, w)) in labels.iter().zip(widths).enumerate() {
        let n = i as u8 + 1;
        let r = Rect::from_min_size(pos2(x, 18.0), vec2(w, 36.0));
        let reachable = n == 1 || state.versions.last().is_some() || state.editing.is_some();
        let resp = ui.interact(r, Id::new(("step", i)), Sense::click());
        let on = state.step == n;
        let painter = ui.painter();
        painter.rect(
            r,
            CornerRadius::same(18),
            if on { c([0x1E, 0x19, 0x10]) } else { egui::Color32::TRANSPARENT },
            Stroke::new(1.0, if on { c([0x5A, 0x44, 0x24]) } else { c(theme::BORDER) }),
            StrokeKind::Inside,
        );
        let dot = pos2(r.min.x + 18.0, r.center().y);
        painter.circle_filled(dot, 12.0, if on { c(theme::WORKING) } else { c([0x1A, 0x1E, 0x28]) });
        painter.text(
            dot,
            Align2::CENTER_CENTER,
            n.to_string(),
            theme::mono(12.0),
            if on { c([0x1C, 0x14, 0x08]) } else { c(theme::TEXT_2) },
        );
        painter.text(
            pos2(r.min.x + 38.0, r.center().y),
            Align2::LEFT_CENTER,
            *text,
            theme::ui(14.0),
            if on { c([0xF3, 0xE2, 0xC6]) } else { c([0x8A, 0x91, 0xA0]) },
        );
        if resp.clicked() && reachable {
            state.step = n;
        }
        x += w + 8.0;
    }

    let next_label = match (state.editing.is_some(), state.step) {
        (true, _) => "Save changes",
        (false, 3) => "Create agent",
        _ => "Continue",
    };
    let next = Rect::from_min_size(pos2(screen.max.x - 24.0 - 150.0, 14.0), vec2(150.0, 44.0));
    let can_continue = match state.step {
        1 => state.versions.last().is_some() && !state.name.trim().is_empty(),
        _ => true,
    };
    let mut create = false;
    if can_continue {
        if button(ui, next, next_label, ButtonStyle::Primary).clicked() {
            if state.step == 3 || state.editing.is_some() {
                create = true;
            } else {
                state.step += 1;
            }
        }
    } else {
        let painter = ui.painter();
        painter.rect(next, CornerRadius::same(10), c([0x1A, 0x1E, 0x28]), Stroke::NONE, StrokeKind::Inside);
        painter.text(next.center(), Align2::CENTER_CENTER, next_label, theme::ui_bold(14.5), c([0x5B, 0x61, 0x70]));
        if ui.interact(next, Id::new("next-disabled"), Sense::hover()).hovered() {
            let why = if state.versions.is_empty() { "Describe the look first" } else { "Give the agent a name" };
            egui::Tooltip::always_open(ui.ctx().clone(), ui.layer_id(), Id::new("next-why"), egui::PopupAnchor::Pointer).show(|ui| {
                ui.label(why);
            });
        }
    }
    let cancel_r = Rect::from_min_size(pos2(next.min.x - 12.0 - 96.0, 14.0), vec2(96.0, 44.0));
    if button(ui, cancel_r, "Cancel", ButtonStyle::Quiet).clicked() {
        cancel = true;
    }
    let _ = core;
    (cancel, create)
}

fn step_look(ui: &mut Ui, core: &mut Core, state: &mut BuilderState, body: Rect) {
    let left = Rect::from_min_size(body.min, vec2(600.0, body.height()));
    let right = Rect::from_min_max(pos2(left.max.x + 16.0, body.min.y), body.max);

    // Left: the conversation with the builder.
    let painter = ui.painter();
    super::panel(painter, left, 18);
    painter.text(left.min + vec2(20.0, 20.0), Align2::LEFT_TOP, "Describe your agent", theme::ui_bold(22.0), c(theme::TEXT));
    let sub = painter.layout(
        "The builder turns your words into a figure script. Change anything by asking.".into(),
        theme::ui(14.0),
        c(theme::TEXT_2),
        380.0,
    );
    painter.galley(left.min + vec2(20.0, 50.0), sub, c(theme::TEXT_2));
    label(painter, pos2(left.max.x - 170.0, left.min.y + 18.0), "NAME");
    let name_rect = Rect::from_min_size(pos2(left.max.x - 170.0, left.min.y + 36.0), vec2(150.0, 40.0));
    painter.rect(name_rect, CornerRadius::same(12), c([0x16, 0x1A, 0x23]), Stroke::new(1.0, c([0x2A, 0x30, 0x3D])), StrokeKind::Inside);
    let mut name_ui = ui.new_child(egui::UiBuilder::new().max_rect(name_rect.shrink2(vec2(12.0, 9.0))));
    name_ui.add(
        egui::TextEdit::singleline(&mut state.name)
            .frame(egui::Frame::NONE)
            .hint_text("Name")
            .font(theme::ui_semibold(16.0))
            .desired_width(name_rect.width() - 24.0),
    );
    let painter = ui.painter();
    super::hline(painter, left.min.y + 92.0, left.min.x + 20.0, left.max.x - 20.0);

    let input_h = 96.0;
    let chips_h = 44.0;
    let chat =
        Rect::from_min_max(pos2(left.min.x + 20.0, left.min.y + 104.0), pos2(left.max.x - 20.0, left.max.y - input_h - chips_h - 16.0));
    let mut chat_ui = ui.new_child(egui::UiBuilder::new().max_rect(chat).layout(egui::Layout::top_down(egui::Align::Min)));
    egui::ScrollArea::vertical().id_salt("builder-chat").stick_to_bottom(true).auto_shrink([false, false]).show(&mut chat_ui, |ui| {
        let width = chat.width() - 14.0;
        if state.messages.is_empty() {
            ui.label(
                egui::RichText::new("Describe how your agent should look, for example a small mail agent, egg shaped, ochre felt, with a blue postal cap and a satchel.")
                    .font(theme::ui(14.5))
                    .color(c(theme::TEXT_2)),
            );
        }
        for (role, text) in &state.messages {
            let m = crate::world::ChatMessage { id: String::new(), role: *role, text: text.clone(), done: true };
            super::agent::bubble(ui, &m, width);
            ui.add_space(8.0);
        }
        if let Some(w) = &state.working {
            let dots = ".".repeat(1 + (core.world.time * 2.0) as usize % 3);
            ui.label(egui::RichText::new(format!("{w}{dots}")).font(theme::mono(11.5)).color(c(theme::WORKING)));
        }
    });

    let mut send: Option<String> = None;
    let chips = ["Rounder", "Bigger eyes", "Add a scarf", "Brass instead of felt"];
    let mut x = left.min.x + 20.0;
    let chip_y = left.max.y - input_h - chips_h - 4.0;
    for chip in chips {
        let w = ui.painter().layout_no_wrap(chip.to_string(), theme::ui(13.0), c(theme::TEXT)).size().x + 26.0;
        let r = Rect::from_min_size(pos2(x, chip_y), vec2(w, 34.0));
        if state.versions.last().is_some() && button(ui, r, chip, ButtonStyle::Quiet).clicked() {
            send = Some(chip.to_string());
        }
        x += w + 8.0;
    }
    let input = Rect::from_min_max(pos2(left.min.x + 20.0, left.max.y - input_h), pos2(left.max.x - 20.0, left.max.y - 16.0));
    let painter = ui.painter();
    painter.rect(input, CornerRadius::same(12), c([0x16, 0x1A, 0x23]), Stroke::new(1.0, c([0x2A, 0x30, 0x3D])), StrokeKind::Inside);
    let busy = state.working.is_some();
    let connected = core.ui.daemon_connected;
    let mut input_ui =
        ui.new_child(egui::UiBuilder::new().max_rect(Rect::from_min_max(input.min + vec2(14.0, 12.0), input.max - vec2(110.0, 10.0))));
    input_ui.add_enabled_ui(!busy && connected, |ui| {
        let hint = if connected { "Describe a change, or the whole agent" } else { "The agent daemon is not running" };
        let resp = ui.add(
            egui::TextEdit::multiline(&mut state.prompt)
                .id(Id::new("builder-prompt"))
                .frame(egui::Frame::NONE)
                .hint_text(egui::RichText::new(hint).color(c([0x6B, 0x72, 0x82])))
                .font(theme::ui(14.5))
                .desired_rows(3)
                .desired_width(input.width() - 130.0)
                .return_key(Some(egui::KeyboardShortcut::new(egui::Modifiers::SHIFT, egui::Key::Enter))),
        );
        if resp.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter) && !i.modifiers.shift) {
            send = Some(state.prompt.trim().to_string());
        }
    });
    let send_r = Rect::from_min_size(pos2(input.max.x - 96.0, input.max.y - 52.0), vec2(84.0, 40.0));
    if !busy && connected && button(ui, send_r, "Send", ButtonStyle::Primary).clicked() {
        send = Some(state.prompt.trim().to_string());
    }
    if let Some(text) = send.filter(|t| !t.is_empty()) {
        state.prompt.clear();
        state.messages.push((Role::User, text.clone()));
        state.working = Some("The builder is drawing".into());
        let script = state.versions.last().cloned();
        if let Some(link) = &core.link {
            link.send(&Command::Build { session: state.session.clone(), prompt: text, script });
        }
    }

    preview_pane(ui, core, state, right);
}

fn preview_pane(ui: &mut Ui, core: &mut Core, state: &mut BuilderState, rect: Rect) {
    let painter = ui.painter();
    super::panel(painter, rect, 18);
    painter.text(rect.min + vec2(20.0, 22.0), Align2::LEFT_TOP, "Preview", theme::ui_bold(17.0), c(theme::TEXT));
    let version = if state.versions.is_empty() { "STARTER".to_string() } else { format!("VERSION {}", state.versions.len()) };
    painter.text(rect.min + vec2(98.0, 26.0), Align2::LEFT_TOP, version, theme::mono(11.0), c(theme::TEXT_2));
    if !state.versions.is_empty() {
        let undo = Rect::from_min_size(rect.min + vec2(196.0, 16.0), vec2(80.0, 32.0));
        if button(ui, undo, "Undo", ButtonStyle::Quiet).clicked() {
            state.undo();
        }
    }
    let seg = Rect::from_min_size(pos2(rect.max.x - 20.0 - 170.0, rect.min.y + 14.0), vec2(170.0, 38.0));
    ui.painter().rect(seg, CornerRadius::same(12), c([0x0E, 0x11, 0x17]), Stroke::new(1.0, c(theme::BORDER)), StrokeKind::Inside);
    let half = Rect::from_min_size(seg.min + vec2(3.0, 3.0), vec2(seg.width() / 2.0 - 3.0, seg.height() - 6.0));
    if button(ui, half, "Figure", if state.show_script { ButtonStyle::Ghost } else { ButtonStyle::Selected }).clicked() {
        state.show_script = false;
    }
    if button(
        ui,
        half.translate(vec2(half.width(), 0.0)),
        "Script",
        if state.show_script { ButtonStyle::Selected } else { ButtonStyle::Ghost },
    )
    .clicked()
    {
        state.show_script = true;
    }

    let stage = Rect::from_min_max(pos2(rect.min.x + 20.0, rect.min.y + 66.0), pos2(rect.max.x - 20.0, rect.max.y - 190.0));
    if state.show_script {
        let painter = ui.painter();
        painter.rect(stage, CornerRadius::same(14), c(theme::TERMINAL_BG), Stroke::new(1.0, c([0x1E, 0x23, 0x2D])), StrokeKind::Inside);
        let mut code_ui = ui.new_child(egui::UiBuilder::new().max_rect(stage.shrink(16.0)));
        egui::ScrollArea::both().id_salt("builder-script").auto_shrink([false, false]).show(&mut code_ui, |ui| {
            ui.add(egui::Label::new(egui::RichText::new(state.script()).font(theme::mono(12.5)).color(c([0xC9, 0xCE, 0xD9]))).extend());
        });
    } else {
        let ppp = ui.ctx().pixels_per_point();
        state.preview_px = ((stage.width() * ppp) as u32, (stage.height() * ppp) as u32);
        let resp = ui.interact(stage, Id::new("builder-stage"), Sense::drag());
        if resp.dragged() {
            state.yaw += resp.drag_delta().x * 0.012;
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
        } else if resp.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
        }
        if let Some(tex) = core.preview_texture() {
            ui.painter().image(tex, stage, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), egui::Color32::WHITE);
        }
        ui.painter().text(
            pos2(stage.center().x, stage.max.y - 14.0),
            Align2::CENTER_CENTER,
            "DRAG TO TURN",
            theme::mono(11.0),
            c([0x6B, 0x72, 0x82]),
        );
    }

    // Animations (D5, D9) and the states the agent can set (D24).
    let y = stage.max.y + 18.0;
    let painter = ui.painter();
    label(painter, pos2(rect.min.x + 20.0, y), "ANIMATIONS");
    let anims = [
        (PreviewAnim::Working, "Working", theme::WORKING),
        (PreviewAnim::NeedsYou, "Needs you", theme::NEEDS_YOU),
        (PreviewAnim::Resting, "Resting", theme::RESTING),
        (PreviewAnim::Walk, "Walk", [0xC9, 0xCE, 0xD9]),
        (PreviewAnim::Hop, "Hop", [0xC9, 0xCE, 0xD9]),
    ];
    let mut x = rect.min.x + 20.0;
    for (anim, text, dot) in anims {
        let w = ui.painter().layout_no_wrap(text.to_string(), theme::ui(14.0), c(theme::TEXT)).size().x + 40.0;
        let r = Rect::from_min_size(pos2(x, y + 20.0), vec2(w, 42.0));
        let on = state.anim == anim;
        let resp = ui.interact(r, Id::new(("anim", text)), Sense::click());
        let painter = ui.painter();
        painter.rect(
            r,
            CornerRadius::same(12),
            if on { c([0x1E, 0x19, 0x10]) } else { c([0x16, 0x1A, 0x23]) },
            Stroke::new(1.0, if on { c([0xC7, 0x96, 0x4A]) } else { c([0x26, 0x2C, 0x38]) }),
            StrokeKind::Inside,
        );
        painter.circle_filled(pos2(r.min.x + 16.0, r.center().y), 4.0, c(dot));
        painter.text(
            pos2(r.min.x + 27.0, r.center().y),
            Align2::LEFT_CENTER,
            text,
            theme::ui(14.0),
            if on { c([0xF3, 0xE2, 0xC6]) } else { c([0xC9, 0xCE, 0xD9]) },
        );
        if resp.clicked() {
            state.anim = anim;
            state.anim_since = core.world.time;
        }
        x = r.max.x + 8.0;
    }

    let params: Vec<(String, ParamKind)> =
        state.figure.as_ref().map(|f| f.params.iter().map(|p| (p.name.clone(), p.kind.clone())).collect()).unwrap_or_default();
    let px = rect.max.x - 20.0 - 230.0;
    let painter = ui.painter();
    label(painter, pos2(px, y), "STATES THE AGENT CAN SET");
    if params.is_empty() {
        painter.text(pos2(px, y + 32.0), Align2::LEFT_TOP, "None yet. Ask for one.", theme::ui(13.5), c(theme::TEXT_2));
    }
    for (i, (name, kind)) in params.iter().enumerate().take(2) {
        let r = Rect::from_min_size(pos2(px, y + 20.0 + i as f32 * 48.0), vec2(230.0, 42.0));
        let ParamKind::Toggle { default } = kind else {
            ui.painter().text(
                r.left_center() + vec2(14.0, 0.0),
                Align2::LEFT_CENTER,
                format!("{name} (set by the agent)"),
                theme::ui(13.5),
                c(theme::TEXT_2),
            );
            continue;
        };
        let on = state.params.get(name).and_then(|v| v.as_bool()).unwrap_or(*default);
        let resp = ui.interact(r, Id::new(("param", name)), Sense::click());
        let painter = ui.painter();
        painter.rect(r, CornerRadius::same(12), c([0x16, 0x1A, 0x23]), Stroke::new(1.0, c([0x26, 0x2C, 0x38])), StrokeKind::Inside);
        painter.text(pos2(r.min.x + 14.0, r.center().y), Align2::LEFT_CENTER, name, theme::ui(14.0), c(theme::TEXT));
        let tog = Rect::from_min_size(pos2(r.max.x - 54.0, r.center().y - 13.0), vec2(44.0, 26.0));
        painter.rect_filled(tog, CornerRadius::same(13), if on { c(theme::WORKING) } else { c([0x3A, 0x40, 0x4D]) });
        painter.circle_filled(pos2(if on { tog.max.x - 13.0 } else { tog.min.x + 13.0 }, tog.center().y), 10.0, c([0xF4, 0xF5, 0xF8]));
        if resp.clicked() {
            state.params.insert(name.clone(), serde_json::Value::Bool(!on));
        }
    }

    let footer_y = rect.max.y - 40.0;
    let painter = ui.painter();
    super::hline(painter, footer_y - 14.0, rect.min.x + 20.0, rect.max.x - 20.0);
    if let Some(f) = &state.figure {
        painter.text(
            pos2(rect.min.x + 20.0, footer_y),
            Align2::LEFT_CENTER,
            format!("{} / {} TRIANGLES", f.triangles, f.budget),
            theme::mono(12.0),
            c(theme::TEXT_2),
        );
        let bar = Rect::from_min_size(pos2(rect.min.x + 210.0, footer_y - 3.0), vec2(120.0, 6.0));
        painter.rect_filled(bar, CornerRadius::same(3), c(theme::BORDER));
        let mut used = bar;
        used.set_width(bar.width() * (f.triangles as f32 / f.budget.max(1) as f32).min(1.0));
        painter.rect_filled(used, CornerRadius::same(3), c(theme::WORKING));
        let mut x =
            painter.text(pos2(rect.min.x + 356.0, footer_y), Align2::LEFT_CENTER, "ANCHORS", theme::mono(12.0), c(theme::TEXT_2)).max.x
                + 14.0;
        for slot in ["ring", "face", "working", "waiting", "resting"] {
            check(painter, pos2(x + 5.0, footer_y));
            x = painter.text(pos2(x + 14.0, footer_y), Align2::LEFT_CENTER, slot, theme::mono(12.0), c([0xC9, 0xCE, 0xD9])).max.x + 14.0;
        }
    }
}

fn step_mind(ui: &mut Ui, core: &mut Core, state: &mut BuilderState, body: Rect) {
    let panel = Rect::from_center_size(pos2(body.center().x, body.min.y + body.height() / 2.0), vec2(760.0, body.height()));
    super::panel(ui.painter(), panel, 18);
    // The form scrolls when its sections do not fit, for example with several workspace folders.
    let mut outer = ui.new_child(egui::UiBuilder::new().max_rect(panel.shrink2(vec2(32.0, 28.0))));
    egui::ScrollArea::vertical().id_salt("builder-mind").auto_shrink([false, false]).show(&mut outer, |form| {
        form.spacing_mut().item_spacing = vec2(10.0, 10.0);
        mind_form(form, core, state);
    });
}

fn mind_form(form: &mut Ui, core: &mut Core, state: &mut BuilderState) {
    let heading = |text: &str| egui::RichText::new(text).font(theme::mono(11.0)).color(c(theme::TEXT_2));
    form.label(egui::RichText::new("Mind and access").font(theme::ui_bold(22.0)));
    form.label(
        egui::RichText::new("Which model drives the agent, what it may do and which windows it gets.")
            .font(theme::ui(14.0))
            .color(c(theme::TEXT_2)),
    );
    form.add_space(6.0);
    form.label(heading("MIND"));
    let kinds = [
        (RuntimeKind::ClaudeCli, "Claude Code CLI"),
        (RuntimeKind::CodexCli, "Codex CLI"),
        (RuntimeKind::Ollama, "Ollama on this machine"),
        (RuntimeKind::Mock, "Mock (played by the scenario)"),
    ];
    form.horizontal(|ui| {
        for (kind, text) in kinds {
            if ui.selectable_label(state.runtime == kind, text).clicked() && state.runtime != kind {
                state.runtime = kind;
                state.custom_model = false;
                state.model = match kind {
                    RuntimeKind::ClaudeCli => "sonnet".into(),
                    _ => String::new(),
                };
            }
        }
    });
    if state.runtime == RuntimeKind::Ollama && state.ollama_models.is_none() {
        let url = meerora_protocol::Settings::load(&core.assets.data_dir)
            .ok()
            .and_then(|s| s.paths.ollama)
            .unwrap_or_else(|| "http://localhost:11434".into());
        state.ollama_models = Some(fetch_ollama_models(&url));
    }
    if state.runtime != RuntimeKind::Mock {
        let ollama: Vec<String> = match &state.ollama_models {
            Some(Ok(list)) => list.clone(),
            _ => Vec::new(),
        };
        if state.runtime == RuntimeKind::Ollama && state.model.is_empty() {
            state.model = ollama.iter().find(|m| m.starts_with("qwen3")).or(ollama.first()).cloned().unwrap_or_default();
        }
        let options = model_options(state.runtime, &ollama);
        let selected = if state.custom_model {
            "Other model".to_string()
        } else {
            options.iter().find(|(v, _)| *v == state.model).map_or_else(|| state.model.clone(), |(_, l)| l.clone())
        };
        form.horizontal(|ui| {
            ui.label(egui::RichText::new("Model").color(c(theme::TEXT_2)));
            egui::ComboBox::from_id_salt("model").width(260.0).selected_text(selected).show_ui(ui, |ui| {
                for (value, text) in &options {
                    if ui.selectable_label(!state.custom_model && state.model == *value, text).clicked() {
                        state.model = value.clone();
                        state.custom_model = false;
                    }
                }
                if ui.selectable_label(state.custom_model, "Other model").clicked() {
                    state.custom_model = true;
                }
            });
            if state.custom_model {
                ui.add(egui::TextEdit::singleline(&mut state.model).hint_text("model name").desired_width(200.0));
            }
        });
        if let Some(Err(e)) = &state.ollama_models
            && state.runtime == RuntimeKind::Ollama
        {
            form.label(egui::RichText::new(e).font(theme::ui(13.0)).color(c([0xE0, 0x67, 0x5A])));
        }
    }
    form.add_space(6.0);
    form.label(heading("INSTRUCTIONS"));
    form.add(
        egui::TextEdit::multiline(&mut state.instructions)
            .hint_text("You check the inbox and tell me about mail that matters. Never delete anything.")
            .desired_rows(3)
            .desired_width(f32::INFINITY),
    );
    form.add_space(6.0);
    form.label(heading("ACCESS"));
    form.label(
        egui::RichText::new("Every agent reads and edits its own directory. Everything else is something you allow here.")
            .font(theme::ui(13.5))
            .color(c(theme::TEXT_2)),
    );
    form.checkbox(&mut state.shell, "Run commands in its own directory");
    form.checkbox(&mut state.web, "Fetch web pages and search the web");
    form.add_space(6.0);
    workspace_list(form, core, state);
    form.add_space(6.0);
    form.label(heading("WINDOWS IT OPENS WITH"));
    form.horizontal_wrapped(|ui| {
        ui.add_enabled(false, egui::Checkbox::new(&mut true, "Chat"));
        for app in App::ALL.into_iter().filter(|a| *a != App::Chat) {
            let mut on = state.windows.contains(&app);
            if ui.checkbox(&mut on, super::AppTab::from_app(app).title()).changed() {
                state.windows.retain(|a| *a != app);
                if on {
                    state.windows.push(app);
                }
            }
        }
    });
    form.add_space(6.0);
    form.label(heading("WHEN A TASK YOU GAVE IS DONE"));
    form.horizontal(|ui| {
        ui.radio_value(&mut state.on_done, OnDone::Wait, "Come to the front with a full ring");
        ui.radio_value(&mut state.on_done, OnDone::Rest, "Go back to rest");
    });
    form.add_space(6.0);
    form.label(heading("HOME STATION"));
    let stations: Vec<String> =
        core.world.space.as_ref().map(|s| s.desc.stations.iter().map(|st| st.id.clone()).collect()).unwrap_or_default();
    egui::ComboBox::from_id_salt("home").selected_text(state.home.replace('_', " ")).show_ui(form, |ui| {
        for s in stations {
            let text = s.replace('_', " ");
            ui.selectable_value(&mut state.home, s, text);
        }
    });
    if let Some(e) = &state.error {
        form.add_space(6.0);
        form.label(egui::RichText::new(e).font(theme::mono(12.0)).color(c([0xE0, 0x67, 0x5A])));
    }
}

fn step_schedule(ui: &mut Ui, core: &mut Core, state: &mut BuilderState, body: Rect) {
    let panel = Rect::from_center_size(pos2(body.center().x, body.min.y + body.height() / 2.0), vec2(760.0, body.height()));
    super::panel(ui.painter(), panel, 18);
    let mut form = ui.new_child(egui::UiBuilder::new().max_rect(panel.shrink2(vec2(32.0, 28.0))));
    form.spacing_mut().item_spacing = vec2(10.0, 10.0);
    form.label(egui::RichText::new("Schedule").font(theme::ui_bold(22.0)));
    form.label(
        egui::RichText::new("Wake the agent on its own, for example every 5 minutes to check mail. Leave it empty for an agent that only works when you ask.")
            .font(theme::ui(14.0))
            .color(c(theme::TEXT_2)),
    );
    let stations: Vec<String> =
        core.world.space.as_ref().map(|s| s.desc.stations.iter().map(|st| st.id.clone()).collect()).unwrap_or_default();
    let mut remove = None;
    for (i, entry) in state.schedule.iter_mut().enumerate() {
        form.add_space(6.0);
        egui::Frame::new().fill(c([0x16, 0x1A, 0x23])).corner_radius(CornerRadius::same(12)).inner_margin(egui::Margin::same(14)).show(
            &mut form,
            |ui| {
                ui.horizontal(|ui| {
                    ui.label("Every");
                    ui.add(egui::TextEdit::singleline(&mut entry.every).hint_text("5m").desired_width(70.0));
                    ui.label("or at");
                    ui.add(egui::TextEdit::singleline(&mut entry.at).hint_text("07:00, 18:30").desired_width(120.0));
                    ui.label("quiet");
                    ui.add(egui::TextEdit::singleline(&mut entry.quiet_hours).hint_text("22:00-07:00").desired_width(110.0));
                    if ui.button("Remove").clicked() {
                        remove = Some(i);
                    }
                });
                ui.add(
                    egui::TextEdit::multiline(&mut entry.task)
                        .hint_text("What to do when it wakes. Empty means: follow the instructions.")
                        .desired_rows(2)
                        .desired_width(f32::INFINITY),
                );
                ui.horizontal(|ui| {
                    ui.label("Station");
                    egui::ComboBox::from_id_salt(("station", i))
                        .selected_text(if entry.station.is_empty() { "home".to_string() } else { entry.station.replace('_', " ") })
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut entry.station, String::new(), "home");
                            for s in &stations {
                                ui.selectable_value(&mut entry.station, s.clone(), s.replace('_', " "));
                            }
                        });
                });
            },
        );
    }
    if let Some(i) = remove {
        state.schedule.remove(i);
    }
    form.add_space(8.0);
    if form.button("Add a wake-up").clicked() {
        state.schedule.push(ScheduleDraft {
            every: "5m".into(),
            at: String::new(),
            task: String::new(),
            station: String::new(),
            quiet_hours: String::new(),
        });
    }
    if let Some(e) = &state.error {
        form.add_space(8.0);
        form.label(egui::RichText::new(e).font(theme::mono(12.0)).color(c([0xE0, 0x67, 0x5A])));
    }
    let data_dir = core.assets.data_dir.display().to_string();
    form.add_space(8.0);
    form.label(
        egui::RichText::new(format!("The agent gets its own directory in {data_dir}/agents."))
            .font(theme::ui(13.0))
            .color(ca(theme::TEXT_2, 200)),
    );
}

/// A small check mark, the fonts have no glyph for it.
fn check(painter: &egui::Painter, center: egui::Pos2) {
    let stroke = Stroke::new(1.6, c([0xC9, 0xCE, 0xD9]));
    painter.line_segment([center + vec2(-4.0, 0.0), center + vec2(-1.0, 3.0)], stroke);
    painter.line_segment([center + vec2(-1.0, 3.0), center + vec2(4.5, -3.5)], stroke);
}

fn slug(name: &str) -> String {
    let s: String = name.trim().to_lowercase().chars().map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' }).collect();
    let s = s.split('-').filter(|p| !p.is_empty()).collect::<Vec<_>>().join("-");
    if s.is_empty() { "agent".into() } else { s }
}

/// Turns the builder fields into an agent config. Fields the builder does
/// not show (budget, system flag, other granted capabilities) come from `base`.
fn config_from(state: &BuilderState, base: Option<&AgentConfig>) -> Result<AgentConfig, String> {
    let mut schedule = Vec::new();
    for e in &state.schedule {
        let every = Some(e.every.trim().to_string()).filter(|s| !s.is_empty());
        let at = Some(e.at.trim().to_string()).filter(|s| !s.is_empty());
        if every.is_none() && at.is_none() {
            return Err("A wake-up needs an interval such as 5m or times such as 07:00. Remove the wake-up you do not need.".into());
        }
        if let Some(every) = &every
            && meerora_protocol::parse_duration(every).is_none()
        {
            return Err(format!("\"{every}\" is not an interval. Use for example 30s, 5m or 1h30m."));
        }
        // Without its own task the agent does what its instructions say.
        let task = Some(e.task.trim()).filter(|t| !t.is_empty()).or(Some(state.instructions.trim()).filter(|t| !t.is_empty()));
        let Some(task) = task else {
            return Err("Tell the agent what to do when it wakes, in the wake-up or in its instructions.".into());
        };
        schedule.push(ScheduleEntry {
            every,
            at,
            task: task.to_string(),
            station: Some(e.station.clone()).filter(|s| !s.is_empty()),
            quiet_hours: Some(e.quiet_hours.trim().to_string()).filter(|s| !s.is_empty()),
        });
    }
    let figure = state.versions.last().ok_or("The agent has no figure yet")?;
    let mut granted: Vec<String> =
        base.map(|b| b.capabilities.granted.iter().filter(|g| *g != "shell" && *g != "web").cloned().collect()).unwrap_or_default();
    if state.shell {
        granted.push("shell".to_string());
    }
    if state.web {
        granted.push("web".to_string());
    }
    // In the order of the launcher, whatever order they were ticked in.
    let mut apps = vec![App::Chat];
    apps.extend(App::ALL.into_iter().filter(|a| *a != App::Chat && state.windows.contains(a)));
    let url = base.and_then(|b| b.runtime.as_ref()).filter(|r| r.kind == state.runtime).and_then(|r| r.url.clone());
    Ok(AgentConfig {
        name: state.name.trim().to_string(),
        color: base.map_or_else(|| figure_color(figure), |b| b.color.clone()),
        figure: base.map_or_else(|| "figure.js".into(), |b| b.figure.clone()),
        home: state.home.clone(),
        instructions: Some(state.instructions.trim().to_string()).filter(|s| !s.is_empty()),
        on_done: state.on_done,
        apps,
        runtime: Some(RuntimeConfig { kind: state.runtime, model: Some(state.model.trim().to_string()).filter(|s| !s.is_empty()), url }),
        schedule,
        budget: base.and_then(|b| b.budget.clone()),
        capabilities: CapabilitiesConfig { system: base.is_some_and(|b| b.capabilities.system), granted },
        external: base.and_then(|b| b.external.clone()),
        role: base.and_then(|b| b.role).or(state.role),
        workspace: state.workspace.clone(),
    })
}

/// Writes `agents/<id>/agent.toml` and `figure.js`. Returns the new id.
fn create_agent(core: &Core, state: &BuilderState) -> Result<String, String> {
    // The personal agent lives in agents/me/ (D26).
    let base = if state.role == Some(AgentRole::Personal) { "me".to_string() } else { slug(state.name.trim()) };
    let agents = core.assets.data_dir.join("agents");
    let mut id = base.clone();
    let mut n = 2;
    while agents.join(&id).exists() {
        id = format!("{base}-{n}");
        n += 1;
    }
    let config = config_from(state, None)?;
    let toml = toml::to_string(&config).map_err(|e| e.to_string())?;
    let dir = agents.join(&id);
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    std::fs::write(dir.join("agent.toml"), format!("# Created with the meerora builder.\n{toml}")).map_err(|e| e.to_string())?;
    std::fs::write(dir.join("figure.js"), state.versions.last().expect("config_from checked the figure")).map_err(|e| e.to_string())?;
    Ok(id)
}

/// Writes the edited config and figure back. Comment lines at the top of
/// agent.toml are kept, other comments are lost when the file is rewritten.
fn save_agent(core: &Core, state: &BuilderState, id: &str) -> Result<(), String> {
    let dir = core.agent_dir(id).ok_or_else(|| format!("{id} no longer exists"))?;
    let config = config_from(state, Some(&dir.config))?;
    let path = dir.path.join("agent.toml");
    let old = std::fs::read_to_string(&path).unwrap_or_default();
    let header: String = old.lines().take_while(|l| l.starts_with('#')).map(|l| format!("{l}\n")).collect();
    let toml = toml::to_string(&config).map_err(|e| e.to_string())?;
    // The agent writes its directory, so neither file is written through a symlink there (safefs).
    meerora_protocol::safefs::replace_file(&path, format!("{header}{toml}").as_bytes()).map_err(|e| format!("{}: {e}", path.display()))?;
    let figure = dir.path.join(&config.figure);
    let script = state.versions.last().expect("config_from checked the figure");
    if std::fs::read_to_string(&figure).ok().as_deref() != Some(script.as_str()) {
        meerora_protocol::safefs::replace(&dir.path, &config.figure, script.as_bytes())
            .map_err(|e| format!("{}: {e}", figure.display()))?;
    }
    Ok(())
}

/// The first colour in the script, used for lists and fallbacks.
fn figure_color(script: &str) -> String {
    script
        .match_indices("\"#")
        .find_map(|(i, _)| {
            let hex = script.get(i + 1..i + 8)?;
            theme::hex_rgb(hex).map(|_| hex.to_string())
        })
        .unwrap_or_else(|| "#E8EAF0".into())
}

/// The workspace folders of step 2: folders the agent may read and write
/// besides its own directory, with Remove, and a path field with Add folder.
fn workspace_list(form: &mut Ui, core: &Core, state: &mut BuilderState) {
    let heading = |text: &str| egui::RichText::new(text).font(theme::mono(11.0)).color(c(theme::TEXT_2));
    form.label(heading("WORKSPACE FOLDERS"));
    form.label(
        egui::RichText::new(
            "Folders it may read and write besides its own directory, such as a code repository. Their .git stays read-only for \
             it, so you review, commit and push its changes yourself in the Git app.",
        )
        .font(theme::ui(13.5))
        .color(c(theme::TEXT_2)),
    );
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let mut remove = None;
    for (i, folder) in state.workspace.iter().enumerate() {
        let checked = meerora_protocol::workspace::check(folder, home.as_deref(), &core.assets.data_dir);
        let shown = folder.display().to_string();
        form.horizontal(|ui| {
            if ui.button("Remove").clicked() {
                remove = Some(i);
            }
            ui.add(egui::Label::new(egui::RichText::new(&shown).font(theme::mono(12.5)).color(c(theme::TEXT))).truncate())
                .on_hover_text(&shown);
        });
        if let Err(why) = &checked {
            form.label(egui::RichText::new(why).font(theme::ui(12.5)).color(c([0xE0, 0x67, 0x5A])));
        }
    }
    if let Some(i) = remove {
        state.workspace.remove(i);
    }
    let mut add = false;
    form.horizontal(|ui| {
        let resp = ui.add(egui::TextEdit::singleline(&mut state.workspace_input).hint_text("~/code/project").desired_width(320.0));
        add = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        add |= ui.button("Add folder").clicked();
    });
    if add && !state.workspace_input.trim().is_empty() {
        let text = state.workspace_input.trim().to_string();
        state.workspace_error = add_workspace(state, &text, home.as_deref(), &core.assets.data_dir).err();
    }
    if let Some(e) = &state.workspace_error {
        form.label(egui::RichText::new(e).font(theme::ui(13.0)).color(c([0xE0, 0x67, 0x5A])));
    }
}

/// Adds the folder `text` to the workspace list, written with `~` for the
/// home, or says why it cannot be a workspace folder.
fn add_workspace(state: &mut BuilderState, text: &str, home: Option<&std::path::Path>, data_dir: &std::path::Path) -> Result<(), String> {
    let folder = meerora_protocol::workspace::check(std::path::Path::new(text), home, data_dir)?;
    let shown = std::path::PathBuf::from(meerora_protocol::workspace::shorten(&folder, home));
    let known = state
        .workspace
        .iter()
        .any(|w| meerora_protocol::workspace::expand(w, home).map(|p| p.canonicalize().unwrap_or(p)) == Some(folder.clone()));
    if !known {
        state.workspace.push(shown);
    }
    state.workspace_input.clear();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Tmp(std::path::PathBuf);

    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Step 2 keeps workspace folders with `~`, refuses what agents must
    /// not get with a reason, and writes them to agent.toml.
    #[test]
    fn workspace_folders_go_to_agent_toml() {
        let tmp = Tmp(std::env::temp_dir().join(format!("meerora-builder-ws-{}", std::process::id())));
        let home = tmp.0.join("home");
        let data = home.join(".local/share/meerora");
        std::fs::create_dir_all(home.join("code/project")).unwrap();
        std::fs::create_dir_all(data.join("agents")).unwrap();
        let home = home.canonicalize().unwrap();
        let mut state = BuilderState::new(0.0, "desk".into());
        state.name = "Ember".into();
        state.versions.push(STARTER.to_string());

        state.workspace_input = "~/code/project".into();
        add_workspace(&mut state, "~/code/project", Some(&home), &data).unwrap();
        assert_eq!(state.workspace, [std::path::PathBuf::from("~/code/project")]);
        assert!(state.workspace_input.is_empty());
        add_workspace(&mut state, &home.join("code/project").display().to_string(), Some(&home), &data).unwrap();
        assert_eq!(state.workspace.len(), 1, "the same folder twice is one folder");
        for (bad, why) in
            [("~", "whole home folder"), ("~/.local/share/meerora", "keys"), ("/", "whole computer"), ("~/nope", "does not exist")]
        {
            let err = add_workspace(&mut state, bad, Some(&home), &data).unwrap_err();
            assert!(err.contains(why), "{bad}: {err}");
        }
        assert_eq!(state.workspace.len(), 1);

        let config = config_from(&state, None).unwrap();
        assert_eq!(config.workspace, [std::path::PathBuf::from("~/code/project")]);
        let toml = toml::to_string(&config).unwrap();
        assert!(toml.contains("workspace = [\"~/code/project\"]"), "{toml}");
        let back: AgentConfig = toml::from_str(&toml).unwrap();
        let dir = meerora_protocol::AgentDir { id: "ember".into(), path: tmp.0.join("ember"), config: back };
        assert_eq!(BuilderState::from_agent(0.0, &dir).workspace, [std::path::PathBuf::from("~/code/project")]);
        state.workspace.clear();
        assert!(!toml::to_string(&config_from(&state, None).unwrap()).unwrap().contains("workspace"), "no folders, no line");
    }
}
