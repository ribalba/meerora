//! Local voice in the UI (D16). Push to talk (F8 unless `[voice] key` says
//! otherwise) or a held microphone button records, whisper transcribes, and
//! the text lands in the field that had focus, ready to send. Replies of an
//! agent are read aloud with Piper when its speaker toggle, `[voice.agents]`
//! or `[voice] speak_replies` say so, and Stop ends the speech. The work runs
//! in meerora-voice on its own threads, and without models or with voice
//! off every control stays hidden.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use egui::{Align2, Color32, CornerRadius, Id, LayerId, Order, Pos2, Rect, Sense, Shape, Stroke, StrokeKind, Ui, pos2, vec2};
use meerora_protocol::Role;
use meerora_voice::download::{Cache, Progress};
use meerora_voice::{Input, Status, Toggles, Voice, VoiceEvent, VoiceSettings};

use super::View;
use crate::engine::Core;
use crate::theme::{self, c, ca};

/// The mic and speaker buttons are this wide, with their gap.
const BUTTON: f32 = 32.0;
const GAP: f32 = 6.0;
/// The red of a running recording.
const RECORDING: [u8; 3] = [0xE0, 0x67, 0x5A];
/// Replies that arrive this long after the daemon connected are its replay
/// of the conversation, not new replies, so they are not read aloud.
const REPLAY_SECONDS: f64 = 3.0;

/// A state the snapshot mode shows without a microphone (`--voice STATE`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Demo {
    Ready,
    Recording,
    Transcribing,
    Speaking,
    Downloading,
}

/// A text field a recording is for.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Field {
    /// "Talk to any agent" in the space view.
    Talk,
    /// The chat input of an agent.
    Chat(String),
    /// Any other focused text field, filled by typing the text into it.
    Other(Id),
}

impl Field {
    fn id(&self) -> Id {
        match self {
            Field::Talk => Id::new("talk-input"),
            Field::Chat(agent) => Id::new(("chat-input", agent.as_str())),
            Field::Other(id) => *id,
        }
    }
}

#[derive(Default)]
pub struct VoiceUi {
    /// Set by the window app with [`enable`]. Snapshots and tests leave voice off.
    wanted: bool,
    started: bool,
    voice: Option<Voice>,
    settings: VoiceSettings,
    key: Option<egui::Key>,
    key_down: bool,
    /// The field of the running recording, by the target name given to the service.
    fields: HashMap<String, Field>,
    next_target: u64,
    /// The microphone button held down, by target name.
    held: Option<String>,
    toggles: Toggles,
    toggles_path: Option<PathBuf>,
    /// Reply ids per agent that were seen, so each reply is spoken once.
    seen: HashMap<String, HashSet<String>>,
    quiet_until: f64,
    connected: bool,
    /// Where each field was drawn this frame, for the indicator.
    anchors: HashMap<String, Rect>,
    status: Option<Status>,
    demo: Option<Demo>,
    shown_level: f32,
    /// Text to type into a field on the next frame, after it got the focus.
    typing: Option<(Id, String)>,
}

impl VoiceUi {
    fn start(&mut self, ctx: &egui::Context, data_dir: &std::path::Path) -> Vec<(String, bool)> {
        self.started = true;
        let mut notes = Vec::new();
        self.settings = match VoiceSettings::load(data_dir) {
            Ok(s) => s,
            Err(e) => {
                notes.push((format!("Voice is off because of an error in meerora.toml ({e})"), true));
                return notes;
            }
        };
        if !self.settings.enabled {
            return notes;
        }
        self.key = egui::Key::from_name(&self.settings.key);
        if self.key.is_none() {
            notes.push((
                format!(
                    "[voice] key = {:?} is not a key name, so push to talk uses {}",
                    self.settings.key,
                    meerora_voice::settings::DEFAULT_KEY
                ),
                true,
            ));
            self.key = egui::Key::from_name(meerora_voice::settings::DEFAULT_KEY);
        }
        let Some(root) = Cache::default_root() else {
            notes.push(("Voice is off, because HOME is not set and the models have no cache folder".into(), true));
            return notes;
        };
        self.toggles_path = Some(data_dir.join("voice.json"));
        self.toggles = Toggles::load(&data_dir.join("voice.json"));
        let voice = Voice::start(self.settings.clone(), Cache::new(root));
        let ctx = ctx.clone();
        voice.set_waker(Arc::new(move || ctx.request_repaint()));
        self.voice = Some(voice);
        notes
    }

    /// True when the microphone button shows.
    fn can_listen(&self) -> bool {
        self.status.as_ref().is_some_and(|s| s.stt_ready)
    }

    /// True when the speaker toggle shows.
    fn can_speak(&self) -> bool {
        self.status.as_ref().is_some_and(|s| s.tts_ready)
    }

    fn key_name(&self) -> String {
        self.key.map_or_else(|| self.settings.key.clone(), |k| k.name().to_string())
    }

    fn target_of(&mut self, field: Field) -> String {
        if let Some((name, _)) = self.fields.iter().find(|(_, f)| **f == field) {
            return name.clone();
        }
        self.next_target += 1;
        let name = match &field {
            Field::Talk => "talk".to_string(),
            Field::Chat(agent) => format!("chat:{agent}"),
            Field::Other(_) => format!("field:{}", self.next_target),
        };
        self.fields.insert(name.clone(), field);
        name
    }

    fn begin(&mut self, field: Field) -> Option<String> {
        let target = self.target_of(field);
        let voice = self.voice.as_ref()?;
        voice.begin(target.clone()).then_some(target)
    }

    fn recording_for(&self, target: &str) -> Option<Input> {
        let s = self.status.as_ref()?;
        (s.input != Input::Idle && s.target.as_deref() == Some(target)).then_some(s.input)
    }

    /// Whether replies of `agent` are read aloud.
    pub fn speaks(&self, agent: &str) -> bool {
        self.settings.speaks(agent, &self.toggles)
    }
}

/// Runs once per frame before the view. It starts voice on the first frame,
/// takes transcripts and notices, handles the push to talk key and reads new
/// replies aloud.
pub fn frame(ui: &mut Ui, core: &mut Core) {
    let ctx = ui.ctx().clone();
    if core.ui.voice.wanted && !core.ui.voice.started {
        let data_dir = core.assets.data_dir.clone();
        for (text, error) in core.ui.voice.start(&ctx, &data_dir) {
            core.ui.toast(core.world.time, text, error);
        }
    }
    let v = &mut core.ui.voice;
    v.anchors.clear();
    v.status = match (v.demo, &v.voice) {
        (Some(demo), _) => Some(demo_status(demo, &core.ui.view, core.world.time)),
        (None, Some(voice)) => Some(voice.status()),
        (None, None) => None,
    };
    if v.voice.is_none() {
        return;
    }
    // Text typed into another field once it has the focus.
    if let Some((id, text)) = v.typing.take() {
        if ctx.memory(|m| m.has_focus(id)) {
            ctx.input_mut(|i| i.events.push(egui::Event::Text(text)));
        } else {
            v.typing = Some((id, text));
        }
    }
    let events = v.voice.as_ref().map(Voice::poll).unwrap_or_default();
    for event in events {
        match event {
            VoiceEvent::Transcript { target, text, .. } => insert(core, &ctx, &target, &text),
            VoiceEvent::Notice { text, error } => core.ui.toast(core.world.time, text, error),
        }
    }
    push_to_talk(ui, core);
    speak_new_replies(core);
}

/// Holding the key records for the focused field. Its key events are taken
/// out, so the terminal or a browser tile never sees them.
fn push_to_talk(ui: &mut Ui, core: &mut Core) {
    let Some(key) = core.ui.voice.key else { return };
    let down = ui.input_mut(|i| {
        i.events.retain(|e| !matches!(e, egui::Event::Key { key: k, .. } if *k == key));
        i.key_down(key)
    });
    let was_down = std::mem::replace(&mut core.ui.voice.key_down, down);
    if down && !was_down {
        let focused = ui.ctx().memory(|m| m.focused());
        let field = match (&core.ui.view, focused) {
            (_, Some(id)) if id == Field::Talk.id() => Some(Field::Talk),
            (View::Agent(agent), Some(id)) if id == Field::Chat(agent.clone()).id() => Some(Field::Chat(agent.clone())),
            (_, Some(id)) => Some(Field::Other(id)),
            (View::Space, None) => Some(Field::Talk),
            (View::Agent(agent), None) => Some(Field::Chat(agent.clone())),
            _ => None,
        };
        match field {
            Some(field) => {
                core.ui.voice.begin(field);
            }
            None => {
                let name = core.ui.voice.key_name();
                core.ui.toast(core.world.time, format!("Click into a text field, then hold {name} to talk"), false);
            }
        }
    } else if !down
        && was_down
        && core.ui.voice.held.is_none()
        && let Some(voice) = &core.ui.voice.voice
    {
        voice.end();
    }
}

/// Puts a transcript into its field, after a space, and gives it the focus.
fn insert(core: &mut Core, ctx: &egui::Context, target: &str, text: &str) {
    let Some(field) = core.ui.voice.fields.get(target).cloned() else { return };
    let append = |s: &mut String| {
        if !s.is_empty() && !s.ends_with(char::is_whitespace) {
            s.push(' ');
        }
        s.push_str(text);
        s.chars().count()
    };
    let id = field.id();
    let end = match &field {
        Field::Talk => append(&mut core.ui.talk),
        Field::Chat(agent) => append(core.ui.drafts.entry(agent.clone()).or_default()),
        Field::Other(_) => {
            core.ui.voice.typing = Some((id, text.to_string()));
            ctx.memory_mut(|m| m.request_focus(id));
            return;
        }
    };
    let mut state = egui::widgets::text_edit::TextEditState::load(ctx, id).unwrap_or_default();
    state.cursor.set_char_range(Some(egui::text::CCursorRange::one(egui::text::CCursor::new(end))));
    state.store(ctx, id);
    ctx.memory_mut(|m| m.request_focus(id));
}

/// Speaks agent replies that finished since the last frame. Replies the
/// daemon replays after it connects are history and stay silent.
fn speak_new_replies(core: &mut Core) {
    let now = core.world.time;
    let v = &mut core.ui.voice;
    if core.ui.daemon_connected && !v.connected {
        v.quiet_until = now + REPLAY_SECONDS;
    }
    v.connected = core.ui.daemon_connected;
    let Some(voice) = &v.voice else { return };
    for a in &core.world.agents {
        let first = !v.seen.contains_key(&a.id);
        let speaks = v.settings.speaks(&a.id, &v.toggles);
        let seen = v.seen.entry(a.id.clone()).or_default();
        for m in a.messages.iter().filter(|m| m.role == Role::Agent && m.done) {
            if seen.insert(m.id.clone()) && !first && now >= v.quiet_until && speaks {
                voice.speak(&m.text, Some(&a.id));
            }
        }
        seen.retain(|id| a.messages.iter().any(|m| &m.id == id));
    }
}

/// Room the chat input leaves for the voice buttons.
pub fn chat_controls_width(core: &Core) -> f32 {
    let v = &core.ui.voice;
    let n = usize::from(v.can_listen()) + usize::from(v.can_speak());
    n as f32 * (BUTTON + GAP)
}

/// The microphone and the speaker toggle at the right end of an agent's
/// chat input. `area` is the room from [`chat_controls_width`], `field`
/// the whole input, where the indicator goes.
pub fn chat_controls(ui: &mut Ui, core: &mut Core, agent: &str, area: Rect, field: Rect) {
    let target = format!("chat:{agent}");
    core.ui.voice.anchors.insert(target, field);
    let mut x = area.min.x + GAP / 2.0;
    let y = area.center().y;
    if core.ui.voice.can_speak() {
        speaker_button(ui, core, agent, Rect::from_center_size(pos2(x + BUTTON / 2.0, y), vec2(BUTTON, BUTTON)));
        x += BUTTON + GAP;
    }
    if core.ui.voice.can_listen() {
        mic_button(ui, core, Field::Chat(agent.to_string()), Rect::from_center_size(pos2(x + BUTTON / 2.0, y), vec2(BUTTON, BUTTON)));
    }
}

/// Room the talk field leaves for the microphone.
pub fn talk_controls_width(core: &Core) -> f32 {
    if core.ui.voice.can_listen() { BUTTON + GAP } else { 0.0 }
}

/// The microphone at the right end of "Talk to any agent".
pub fn talk_controls(ui: &mut Ui, core: &mut Core, area: Rect, field: Rect) {
    core.ui.voice.anchors.insert("talk".into(), field);
    if core.ui.voice.can_listen() {
        mic_button(ui, core, Field::Talk, Rect::from_center_size(area.center(), vec2(BUTTON, BUTTON)));
    }
}

/// Records while it is held down, like the push to talk key.
fn mic_button(ui: &mut Ui, core: &mut Core, field: Field, rect: Rect) {
    let target = core.ui.voice.target_of(field.clone());
    let resp = ui.interact(rect, Id::new(("voice-mic", &target)), Sense::click_and_drag());
    let pressed = resp.is_pointer_button_down_on();
    let v = &mut core.ui.voice;
    if pressed && v.held.is_none() {
        if v.begin(field).is_some() {
            v.held = Some(target.clone());
        }
    } else if !pressed && v.held.as_deref() == Some(target.as_str()) {
        v.held = None;
        if let Some(voice) = &v.voice {
            voice.end();
        }
    }
    let state = v.recording_for(&target);
    let level = v.status.as_ref().map_or(0.0, |s| s.level);
    let key = v.key_name();
    let painter = ui.painter();
    let center = rect.center();
    let r = rect.width() / 2.0;
    match state {
        Some(Input::Recording) => {
            painter.circle_filled(center, r, c(RECORDING));
            // A ring that grows with the level.
            painter.circle_stroke(center, r + 2.0 + level * 5.0, Stroke::new(2.0, ca(RECORDING, 110)));
            mic_glyph(painter, center, rect.width(), c([0x1A, 0x0E, 0x0E]));
        }
        Some(_) => {
            painter.circle_filled(center, r, c([0x1E, 0x23, 0x2E]));
            spinner(painter, center, r - 3.0, ui.input(|i| i.time) as f32, c(theme::NEEDS_YOU));
            mic_glyph(painter, center, rect.width(), c(theme::TEXT_2));
        }
        None => {
            let hovered = resp.hovered();
            painter.circle(
                center,
                r,
                if hovered { c([0x1E, 0x23, 0x2E]) } else { Color32::TRANSPARENT },
                Stroke::new(1.0, c(if hovered { [0x3A, 0x42, 0x52] } else { [0x2A, 0x30, 0x3D] })),
            );
            mic_glyph(painter, center, rect.width(), c(if hovered { theme::TEXT } else { [0xC9, 0xCE, 0xD9] }));
        }
    }
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp.on_hover_text(format!("Hold to talk, or hold {key}"));
}

/// Switches reading this agent's replies aloud. The choice is kept in `voice.json`.
fn speaker_button(ui: &mut Ui, core: &mut Core, agent: &str, rect: Rect) {
    let resp = ui.interact(rect, Id::new(("voice-speaker", agent)), Sense::click());
    let v = &mut core.ui.voice;
    let on = v.speaks(agent);
    let speaking = v.status.as_ref().is_some_and(|s| s.speaking.as_deref() == Some(agent));
    if resp.clicked() {
        v.toggles.speak_replies.insert(agent.to_string(), !on);
        if let Some(path) = &v.toggles_path
            && let Err(e) = v.toggles.save(path)
        {
            tracing::warn!("cannot save {}: {e}", path.display());
        }
        if on
            && speaking
            && let Some(voice) = &v.voice
        {
            voice.stop_speaking();
        }
    }
    let painter = ui.painter();
    let hovered = resp.hovered();
    let fill = if on {
        ca(theme::NEEDS_YOU, 30)
    } else if hovered {
        c([0x1E, 0x23, 0x2E])
    } else {
        Color32::TRANSPARENT
    };
    let stroke = if on { ca(theme::NEEDS_YOU, 120) } else { c(if hovered { [0x3A, 0x42, 0x52] } else { [0x2A, 0x30, 0x3D] }) };
    painter.circle(rect.center(), rect.width() / 2.0, fill, Stroke::new(1.0, stroke));
    let color = if on { c(theme::NEEDS_YOU) } else { c(theme::TEXT_2) };
    speaker_glyph(painter, rect.center(), rect.width(), color, on);
    if hovered {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp.on_hover_text(if on { "Replies are read aloud. Click to stop that" } else { "Read this agent's replies aloud" });
}

/// Draws after the view. The recording, transcribing or download indicator
/// goes next to its field, the speech pill with Stop next to the speaker.
pub fn draw_overlay(ui: &mut Ui, core: &mut Core) {
    let Some(status) = core.ui.voice.status.clone() else { return };
    let screen = ui.ctx().content_rect();
    let time = ui.input(|i| i.time) as f32;
    let v = &mut core.ui.voice;
    v.shown_level = if status.level > v.shown_level { status.level } else { v.shown_level * 0.85 + status.level * 0.15 };
    // The field of the view, which is the agent's chat input or the talk field.
    let view_field = match &core.ui.view {
        View::Agent(id) => v.anchors.get(&format!("chat:{id}")).copied(),
        _ => v.anchors.get("talk").copied(),
    };
    let mut taken = None;
    if status.input != Input::Idle || status.download.is_some() {
        let anchor = status.target.as_ref().and_then(|t| v.anchors.get(t)).copied().or(view_field);
        let mouse = v.held.is_some();
        let key = v.key_name();
        taken = Some(input_pill(
            ui,
            &status,
            v.shown_level,
            anchor,
            screen,
            time,
            if mouse { "Release to stop".into() } else { format!("Release {key} to stop") },
        ));
        ui.ctx().request_repaint();
    }
    if let Some(who) = &status.speaking {
        let name = core.world.agent(who).map(|a| a.name.clone());
        let label = match name {
            Some(name) => format!("{name} is speaking"),
            None => "Speaking".to_string(),
        };
        let anchor = speaker_field(&core.ui.voice, who).or(view_field);
        if speech_pill(ui, &label, status.queued, screen, time, anchor, taken)
            && let Some(voice) = &core.ui.voice.voice
        {
            voice.stop_speaking();
        }
        ui.ctx().request_repaint();
    }
}

/// The chat input of the agent that speaks, when it is on screen.
fn speaker_field(v: &VoiceUi, who: &str) -> Option<Rect> {
    v.anchors.get(&format!("chat:{who}")).copied()
}

/// Where a pill of `size` goes next to `anchor`. It sits above a field low on
/// the screen and below one near the top, at `x` (0 left edge, 0.5 centre, 1 right
/// edge). Without a field it sits at the bottom centre.
fn place(size: egui::Vec2, anchor: Option<Rect>, x: f32, screen: Rect) -> Rect {
    let pos = match anchor {
        Some(a) => {
            let left = a.min.x + (a.width() - size.x) * x;
            if a.min.y > screen.height() / 2.0 { pos2(left, a.min.y - size.y - 10.0) } else { pos2(left, a.max.y + 10.0) }
        }
        None => pos2(screen.center().x - size.x / 2.0, screen.max.y - size.y - 96.0),
    };
    let pos =
        pos2(pos.x.clamp(screen.min.x + 8.0, screen.max.x - size.x - 8.0), pos.y.clamp(screen.min.y + 8.0, screen.max.y - size.y - 8.0));
    Rect::from_min_size(pos, size)
}

fn input_pill(ui: &Ui, status: &Status, level: f32, anchor: Option<Rect>, screen: Rect, time: f32, hint: String) -> Rect {
    let painter = ui.ctx().layer_painter(LayerId::new(Order::Foreground, Id::new("voice-indicator")));
    let (text, accent, width) = match (&status.download, status.input) {
        (Some(p), Input::Idle) => (download_text(p), theme::NEEDS_YOU, 360.0),
        (_, Input::Recording) => (format!("Listening  {}", clock(status.recording_for)), RECORDING, 330.0),
        _ => ("Transcribing…".to_string(), theme::NEEDS_YOU, 200.0),
    };
    let rect = place(vec2(width, 40.0), anchor, 0.5, screen);
    painter.rect(rect, CornerRadius::same(20), c([0x16, 0x1A, 0x23]), Stroke::new(1.0, ca(accent, 120)), StrokeKind::Inside);
    let mut x = rect.min.x + 18.0;
    let y = rect.center().y;
    match (&status.download, status.input) {
        (Some(p), Input::Idle) => {
            spinner(&painter, pos2(x + 4.0, y), 6.0, time, c(accent));
            x += 18.0;
            let t = painter.text(pos2(x, y), Align2::LEFT_CENTER, &text, theme::mono(11.5), c(theme::TEXT));
            let bar = Rect::from_min_max(pos2(t.max.x + 12.0, y - 2.0), pos2(rect.max.x - 18.0, y + 2.0));
            if bar.width() > 10.0 {
                painter.rect_filled(bar, CornerRadius::same(2), c(theme::BORDER));
                let mut done = bar;
                done.set_width(bar.width() * p.fraction());
                painter.rect_filled(done, CornerRadius::same(2), c(accent));
            }
        }
        (_, Input::Recording) => {
            let pulse = 0.6 + 0.4 * (time * 4.0).sin().abs();
            painter.circle_filled(pos2(x + 4.0, y), 5.0, ca(accent, (255.0 * pulse) as u8));
            x += 18.0;
            let t = painter.text(pos2(x, y), Align2::LEFT_CENTER, &text, theme::mono(11.5), c(theme::TEXT));
            x = t.max.x + 12.0;
            level_bars(&painter, pos2(x, y), level, time, c(accent));
            painter.text(pos2(rect.max.x - 18.0, y), Align2::RIGHT_CENTER, hint, theme::mono(10.5), c(theme::TEXT_2));
        }
        _ => {
            spinner(&painter, pos2(x + 4.0, y), 6.0, time, c(accent));
            painter.text(pos2(x + 18.0, y), Align2::LEFT_CENTER, &text, theme::mono(11.5), c(theme::TEXT));
        }
    }
    rect
}

/// Returns true when Stop was pressed.
fn speech_pill(ui: &mut Ui, label: &str, queued: usize, screen: Rect, time: f32, anchor: Option<Rect>, taken: Option<Rect>) -> bool {
    let size = vec2(300.0, 44.0);
    let mut rect = place(size, anchor, 1.0, screen);
    // Out of the way of the recording indicator, further from the field.
    if let Some(t) = taken
        && t.intersects(rect)
    {
        let up = anchor.is_none_or(|a| rect.center().y < a.center().y);
        rect = rect.translate(vec2(0.0, if up { t.min.y - rect.max.y - 8.0 } else { t.max.y - rect.min.y + 8.0 }));
    }
    let mut stop = false;
    egui::Area::new(Id::new("voice-speaking")).fade_in(false).fixed_pos(rect.min).order(Order::Foreground).show(ui.ctx(), |ui| {
        ui.allocate_exact_size(size, Sense::hover());
        let painter = ui.painter();
        painter.rect(rect, CornerRadius::same(22), c([0x16, 0x1A, 0x23]), Stroke::new(1.0, ca(theme::NEEDS_YOU, 120)), StrokeKind::Inside);
        let y = rect.center().y;
        speaker_glyph(painter, pos2(rect.min.x + 26.0, y), 26.0, c(theme::NEEDS_YOU), true);
        let wave = 0.5 + 0.5 * (time * 3.0).sin();
        painter.circle_filled(pos2(rect.min.x + 44.0, y), 2.0 + wave, ca(theme::NEEDS_YOU, 160));
        let text = if queued > 0 { format!("{label} (+{queued})") } else { label.to_string() };
        let text = super::fit(painter, &text, theme::ui(14.0), size.x - 140.0);
        painter.text(pos2(rect.min.x + 56.0, y), Align2::LEFT_CENTER, text, theme::ui(14.0), c(theme::TEXT));
        let b = Rect::from_min_size(pos2(rect.max.x - 76.0, y - 15.0), vec2(64.0, 30.0));
        stop = super::button(ui, b, "Stop", super::ButtonStyle::Quiet).on_hover_text("Stop reading aloud").clicked();
    });
    stop
}

fn download_text(p: &Progress) -> String {
    format!("Voice models {}%", (p.fraction() * 100.0).round() as i32)
}

fn clock(d: Duration) -> String {
    let s = d.as_secs();
    format!("{}:{:02}", s / 60, s % 60)
}

/// Five bars that follow the level.
fn level_bars(painter: &egui::Painter, left: Pos2, level: f32, time: f32, color: Color32) {
    for (i, shape) in [0.55f32, 0.8, 1.0, 0.8, 0.55].iter().enumerate() {
        let wobble = 0.85 + 0.15 * (time * 9.0 + i as f32 * 1.7).sin();
        let h = 3.0 + 16.0 * (level * shape * wobble).clamp(0.0, 1.0);
        let x = left.x + i as f32 * 6.0;
        painter.rect_filled(Rect::from_center_size(pos2(x + 1.5, left.y), vec2(3.0, h)), CornerRadius::same(1), color);
    }
}

fn spinner(painter: &egui::Painter, center: Pos2, r: f32, time: f32, color: Color32) {
    let start = time * 5.0;
    let points: Vec<Pos2> =
        (0..=20).map(|k| start + 4.0 * k as f32 / 20.0).map(|a| pos2(center.x + r * a.cos(), center.y + r * a.sin())).collect();
    painter.add(Shape::line(points, Stroke::new(2.0, color)));
}

/// A microphone drawn with lines, so no icon font is needed.
fn mic_glyph(painter: &egui::Painter, center: Pos2, size: f32, color: Color32) {
    let s = size * 0.5;
    let body = Rect::from_min_max(pos2(center.x - s * 0.2, center.y - s * 0.52), pos2(center.x + s * 0.2, center.y + s * 0.12));
    painter.rect_filled(body, CornerRadius::same((s * 0.2) as u8), color);
    let arc: Vec<Pos2> = (0..=16)
        .map(|k| std::f32::consts::PI * k as f32 / 16.0)
        .map(|a| pos2(center.x + s * 0.36 * a.cos(), center.y - s * 0.08 + s * 0.36 * a.sin()))
        .collect();
    let stroke = Stroke::new(1.6, color);
    painter.add(Shape::line(arc, stroke));
    painter.line_segment([pos2(center.x, center.y + s * 0.28), pos2(center.x, center.y + s * 0.48)], stroke);
    painter.line_segment([pos2(center.x - s * 0.2, center.y + s * 0.48), pos2(center.x + s * 0.2, center.y + s * 0.48)], stroke);
}

/// A speaker with sound waves when `on`, crossed out when off.
fn speaker_glyph(painter: &egui::Painter, center: Pos2, size: f32, color: Color32, on: bool) {
    let s = size * 0.5;
    let p = |x: f32, y: f32| pos2(center.x + x * s, center.y + y * s);
    painter.rect_filled(Rect::from_min_max(p(-0.55, -0.18), p(-0.28, 0.18)), CornerRadius::ZERO, color);
    painter.add(Shape::convex_polygon(vec![p(-0.3, -0.18), p(0.0, -0.45), p(0.0, 0.45), p(-0.3, 0.18)], color, Stroke::NONE));
    let stroke = Stroke::new(1.5, color);
    if on {
        for r in [0.25f32, 0.45] {
            let arc: Vec<Pos2> = (0..=10).map(|k| -0.8 + 1.6 * k as f32 / 10.0).map(|a| p(0.08 + r * a.cos(), r * a.sin())).collect();
            painter.add(Shape::line(arc, stroke));
        }
    } else {
        painter.line_segment([p(0.15, -0.22), p(0.55, 0.22)], stroke);
        painter.line_segment([p(0.15, 0.22), p(0.55, -0.22)], stroke);
    }
}

/// Starts local voice with the next frame, as `[voice]` of `meerora.toml` says.
pub fn enable(core: &mut Core) {
    core.ui.voice.wanted = true;
}

/// In snapshot mode voice stays off unless `--voice` asks for one of the
/// states, which then shows without a microphone or models.
pub fn snapshot(core: &mut Core, state: Option<&str>) -> anyhow::Result<()> {
    let Some(state) = state else { return Ok(()) };
    let v = &mut core.ui.voice;
    let demo = match state {
        "ready" => Demo::Ready,
        "recording" => Demo::Recording,
        "transcribing" => Demo::Transcribing,
        "speaking" => Demo::Speaking,
        "downloading" => Demo::Downloading,
        _ => anyhow::bail!("--voice is ready, recording, transcribing, speaking or downloading"),
    };
    v.demo = Some(demo);
    v.key = egui::Key::from_name(meerora_voice::settings::DEFAULT_KEY);
    if let View::Agent(id) = &core.ui.view {
        v.target_of(Field::Chat(id.clone()));
        if demo == Demo::Speaking {
            v.toggles.speak_replies.insert(id.clone(), true);
        }
    } else {
        v.target_of(Field::Talk);
    }
    Ok(())
}

fn demo_status(demo: Demo, view: &View, time: f64) -> Status {
    let target = match view {
        View::Agent(id) => format!("chat:{id}"),
        _ => "talk".to_string(),
    };
    let input = match demo {
        Demo::Recording => Input::Recording,
        Demo::Transcribing => Input::Transcribing,
        _ => Input::Idle,
    };
    Status {
        input,
        target: (input != Input::Idle).then_some(target),
        recording_for: Duration::from_secs(3),
        level: if demo == Demo::Recording { 0.7 + 0.1 * (time as f32 * 3.0).sin() } else { 0.0 },
        speaking: match (demo, view) {
            (Demo::Speaking, View::Agent(id)) => Some(id.clone()),
            (Demo::Speaking, _) => Some(String::new()),
            _ => None,
        },
        queued: 0,
        download: (demo == Demo::Downloading).then(|| Progress {
            label: "whisper base model".into(),
            done: 81_000_000,
            total: Some(147_951_465),
            index: 1,
            count: 6,
        }),
        stt_ready: demo != Demo::Downloading,
        tts_ready: demo != Demo::Downloading,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_field_has_the_id_of_the_chat_input() {
        // ui/agent.rs builds the id from a String, voice from a str.
        let agent = String::from("wren");
        assert_eq!(Field::Chat("wren".into()).id(), Id::new(("chat-input", &agent)));
        assert_eq!(Field::Talk.id(), Id::new("talk-input"));
    }

    #[test]
    fn pills_sit_above_low_fields_and_below_high_ones() {
        let screen = Rect::from_min_size(Pos2::ZERO, vec2(1440.0, 900.0));
        let size = vec2(300.0, 40.0);
        let low = Rect::from_min_max(pos2(100.0, 820.0), pos2(1400.0, 880.0));
        let p = place(size, Some(low), 1.0, screen);
        assert_eq!(p.max.y, low.min.y - 10.0);
        assert_eq!(p.max.x, low.max.x);
        let high = Rect::from_min_max(pos2(440.0, 10.0), pos2(1000.0, 54.0));
        let p = place(size, Some(high), 0.5, screen);
        assert_eq!(p.min.y, high.max.y + 10.0);
        assert_eq!(p.center().x, high.center().x);
        // Never off screen.
        let p = place(size, Some(Rect::from_min_max(pos2(1300.0, 10.0), pos2(1440.0, 40.0))), 1.0, screen);
        assert!(screen.contains_rect(p));
        let p = place(size, None, 0.5, screen);
        assert!(screen.contains_rect(p) && p.center().x == screen.center().x);
    }

    #[test]
    fn start_follows_the_voice_settings() {
        let dir = std::env::temp_dir().join(format!("meerora-voice-ui-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ctx = egui::Context::default();
        std::fs::write(dir.join("meerora.toml"), "[voice]\nenabled = false\n").unwrap();
        let mut v = VoiceUi::default();
        assert!(v.start(&ctx, &dir).is_empty());
        assert!(v.voice.is_none(), "voice off starts nothing");
        std::fs::write(dir.join("meerora.toml"), "[voice]\nkey = \"Banana\"\n").unwrap();
        let mut v = VoiceUi::default();
        let notes = v.start(&ctx, &dir);
        assert!(notes[0].0.contains("Banana") && notes[0].1, "{notes:?}");
        assert_eq!(v.key, Some(egui::Key::F8));
        assert!(v.voice.is_some());
        std::fs::write(dir.join("meerora.toml"), "[voice]\nenable = true\n").unwrap();
        let mut v = VoiceUi::default();
        assert!(v.start(&ctx, &dir)[0].0.starts_with("Voice is off"));
        assert!(v.voice.is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn snapshot_states_are_checked() {
        assert_eq!(demo_status(Demo::Recording, &View::Agent("claude".into()), 0.0).target.as_deref(), Some("chat:claude"));
        assert_eq!(demo_status(Demo::Transcribing, &View::Space, 0.0).target.as_deref(), Some("talk"));
        let ready = demo_status(Demo::Ready, &View::Space, 0.0);
        assert!(ready.stt_ready && ready.tts_ready && ready.target.is_none());
        assert!(!demo_status(Demo::Downloading, &View::Space, 0.0).stt_ready);
        assert_eq!(demo_status(Demo::Speaking, &View::Agent("wren".into()), 0.0).speaking.as_deref(), Some("wren"));
    }
}
