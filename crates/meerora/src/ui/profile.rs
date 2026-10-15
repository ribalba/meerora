//! The personal profile (D26) in two steps. The interview: an LLM asks one
//! question at a time in a chat and fills the profile, which grows on the
//! right. The review: every field with a switch for public, friends or
//! private, editable values, photos, and Publish, which pushes the profile
//! to the index without its private fields. No screen is designed for this
//! yet (Q14), so it follows the builder (screen 3).
//!
//! Text shown here never cites decision numbers, the user does not know them.

use std::path::{Path, PathBuf};

use egui::{Align2, CornerRadius, Id, LayerId, Order, Rect, Sense, Stroke, StrokeKind, Ui, pos2, vec2};
use meerora_protocol::{
    Command, DraftStatus, Education, Event, InterviewTurn, Profile, ProfileLink, ProfilePhoto, ProfileText, Role, Visibility, Work,
};
use serde::Deserialize;

use super::feed::{ago, facts, visibility_switch};
use super::index::{MINT, picture, small_pill};
use super::{ButtonStyle, View, button, logo};
use crate::engine::Core;
use crate::theme::{self, c, ca};

const RED: [u8; 3] = [0xE0, 0x67, 0x5A];
/// The largest photo, the same limit as the index.
const PHOTO_MAX: u64 = 2 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Interview,
    Review,
}

pub struct ProfileView {
    pub step: Step,
    session: String,
    /// The personal agent.
    agent: String,
    turns: Vec<InterviewTurn>,
    question: Option<String>,
    answer: String,
    working: Option<String>,
    failed: Option<String>,
    done: bool,
    /// The profile being built and edited.
    pub draft: Profile,
    /// Edited since the last save.
    dirty: bool,
    publishing: bool,
    handle: String,
    creating_account: bool,
    photo_path: String,
    error: Option<String>,
    /// Where Back goes.
    back: View,
}

/// Opens the interview, or the review when the profile has content.
pub fn open_for(core: &mut Core, agent: &str) {
    let draft = core.ui.social.own_profile.clone().unwrap_or_default();
    let step = if draft.is_empty() { Step::Interview } else { Step::Review };
    let back = if core.ui.view == View::Feed { View::Feed } else { View::Space };
    let view = ProfileView {
        step,
        session: format!("interview-{}", (core.world.time * 1000.0) as u64),
        agent: agent.to_string(),
        turns: Vec::new(),
        question: None,
        answer: String::new(),
        working: None,
        failed: None,
        done: false,
        draft,
        dirty: false,
        publishing: false,
        handle: String::new(),
        creating_account: false,
        photo_path: String::new(),
        error: None,
        back,
    };
    core.ui.social.profile = Some(view);
    core.ui.view = View::Profile;
    if step == Step::Interview {
        ask(core);
    }
}

/// Sends the interview so far and asks for the next question.
fn ask(core: &mut Core) {
    let Some(view) = core.ui.social.profile.as_mut() else { return };
    view.working = Some("Thinking about the next question".into());
    view.failed = None;
    let command = Command::Interview {
        session: view.session.clone(),
        agent: view.agent.clone(),
        turns: view.turns.clone(),
        profile: Some(view.draft.clone()),
    };
    match &core.link {
        Some(link) if core.ui.daemon_connected => link.send(&command),
        _ => {
            view.working = None;
            view.failed = Some("The agent daemon is not running, so nobody can ask.".into());
        }
    }
}

/// An `InterviewStep` from the daemon.
pub fn on_step(core: &mut Core, event: &Event) {
    let Event::InterviewStep { session, status, question, profile, done, message } = event else { return };
    let Some(view) = core.ui.social.profile.as_mut().filter(|v| v.session == *session) else { return };
    match status {
        DraftStatus::Working => view.working = Some(message.clone()),
        DraftStatus::Failed => {
            view.working = None;
            view.failed = Some(message.clone());
        }
        DraftStatus::Ready => {
            view.working = None;
            view.question = question.clone();
            view.done = *done;
            if let Some(p) = profile {
                // The interview never touches photos.
                let photos = std::mem::take(&mut view.draft.photos);
                view.draft = (**p).clone();
                if view.draft.photos.is_empty() {
                    view.draft.photos = photos;
                }
                // Kept on this computer as it grows.
                let save = Command::SaveProfile { profile: view.draft.clone(), publish: false };
                if let Some(link) = &core.link {
                    link.send(&save);
                }
            }
        }
    }
}

/// An `OwnProfile` from the daemon.
pub fn on_own_profile(core: &mut Core, published: bool, error: Option<String>) {
    let now = core.world.time;
    let own = core.ui.social.own_profile.clone();
    let Some(view) = core.ui.social.profile.as_mut() else { return };
    if view.publishing && (published || error.is_some()) {
        view.publishing = false;
        if published && error.is_none() {
            core.ui.toast(now, "Your profile is published. People find you by name in the index now.", false);
        }
    }
    let Some(view) = core.ui.social.profile.as_mut() else { return };
    view.error = error;
    if view.error.is_none() {
        view.dirty = false;
    }
    if !view.dirty
        && let Some(own) = own
        && view.working.is_none()
    {
        view.draft = own;
    }
}

/// Fills the interview for a snapshot from a JSON file with `turns`,
/// `question` and `profile`.
pub fn snapshot_interview(core: &mut Core, path: &Path) -> anyhow::Result<()> {
    #[derive(Deserialize)]
    struct Fixture {
        turns: Vec<InterviewTurn>,
        question: Option<String>,
        profile: Option<Profile>,
    }
    let text = std::fs::read_to_string(path)?;
    let fixture: Fixture = serde_json::from_str(&text)?;
    let agent = super::feed::personal_agent(core).ok_or_else(|| anyhow::anyhow!("no personal agent in this data dir"))?;
    open_for(core, &agent);
    if let Some(view) = core.ui.social.profile.as_mut() {
        view.step = Step::Interview;
        view.turns = fixture.turns;
        view.question = fixture.question;
        view.working = None;
        view.failed = None;
        if let Some(p) = fixture.profile {
            view.draft = p;
        }
    }
    Ok(())
}

/// Switches the open profile to the review, for snapshots.
pub fn snapshot_review(core: &mut Core) {
    if let Some(view) = core.ui.social.profile.as_mut() {
        view.step = Step::Review;
    }
}

/// Starts loading the photos of the open profile, for snapshots.
pub fn request_pictures(core: &mut Core) {
    let Some(view) = core.ui.social.profile.as_ref() else { return };
    let sources: Vec<String> = view.draft.photos.iter().map(|p| photo_source(core, &view.agent, &p.url)).collect();
    for src in sources {
        core.ui.social.pictures.request(&src);
    }
}

/// Where a photo of the profile is: its URL, or the file in the agent's directory.
fn photo_source(core: &Core, agent: &str, url: &str) -> String {
    if url.starts_with("https://") || url.starts_with("http://") {
        return url.to_string();
    }
    core.agent_dir(agent).map(|d| d.path.join(url).display().to_string()).unwrap_or_default()
}

pub fn draw(ui: &mut Ui, core: &mut Core) {
    let Some(mut view) = core.ui.social.profile.take() else {
        core.ui.view = View::Space;
        return;
    };
    core.ui.social.pictures.poll(ui.ctx());
    for photo in &view.draft.photos {
        let src = photo_source(core, &view.agent, &photo.url);
        core.ui.social.pictures.request(&src);
    }
    if view.creating_account && (core.ui.social.handle.is_some() || core.ui.social.account_error.is_some()) {
        view.creating_account = false;
    }
    // Photos dropped on the window.
    if view.step == Step::Review {
        let dropped: Vec<PathBuf> = ui.input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).collect());
        for path in dropped {
            add_photo(core, &mut view, &path);
        }
    }
    let screen = ui.ctx().content_rect();
    ui.ctx().layer_painter(LayerId::background()).rect_filled(screen, CornerRadius::ZERO, c(theme::BG));
    let mut act = Act::None;
    egui::Area::new(Id::new("profile")).fade_in(false).fixed_pos(screen.min).order(Order::Middle).show(ui.ctx(), |ui| {
        ui.allocate_exact_size(screen.size(), Sense::hover());
        act = header(ui, core, &mut view, screen);
        let body = Rect::from_min_max(pos2(24.0, 92.0), pos2(screen.max.x - 24.0, screen.max.y - 24.0));
        match view.step {
            Step::Interview => {
                let left = Rect::from_min_size(body.min, vec2(600.0, body.height()));
                let right = Rect::from_min_max(pos2(left.max.x + 16.0, body.min.y), body.max);
                if let Some(a) = interview_pane(ui, core, &mut view, left) {
                    act = a;
                }
                preview_pane(ui, core, &view, right);
            }
            Step::Review => {
                if let Some(a) = review_pane(ui, core, &mut view, body) {
                    act = a;
                }
            }
        }
    });
    if ui.input(|i| i.key_pressed(egui::Key::Escape)) && !ui.ctx().memory(|m| m.focused().is_some()) {
        act = Act::Back;
    }
    if view.working.is_some() || view.publishing || view.creating_account {
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
    }
    let back = view.back.clone();
    core.ui.social.profile = Some(view);
    match act {
        Act::None => {}
        Act::Back => {
            core.ui.view = back;
            if core.ui.view == View::Feed {
                super::feed::open(core);
            }
        }
        Act::Ask => ask(core),
        Act::Save(publish) => save(core, publish),
        Act::CreateAccount(handle) => {
            core.ui.social.account_error = None;
            if let Some(link) = core.link.as_ref().filter(|_| core.ui.daemon_connected) {
                link.send(&Command::CreateAccount { handle });
                if let Some(v) = core.ui.social.profile.as_mut() {
                    v.creating_account = true;
                }
            }
        }
    }
}

enum Act {
    None,
    Back,
    Ask,
    Save(bool),
    CreateAccount(String),
}

fn save(core: &mut Core, publish: bool) {
    let Some(view) = core.ui.social.profile.as_mut() else { return };
    let problems = view.draft.problems();
    if !problems.is_empty() {
        view.error = Some(format!("Fix these first: {}.", problems.join(". ")));
        return;
    }
    view.error = None;
    view.publishing = publish;
    let command = Command::SaveProfile { profile: view.draft.clone(), publish };
    match &core.link {
        Some(link) if core.ui.daemon_connected => link.send(&command),
        _ => {
            view.publishing = false;
            view.error = Some("The agent daemon is not running.".into());
        }
    }
}

fn header(ui: &mut Ui, core: &Core, view: &mut ProfileView, screen: Rect) -> Act {
    let mut act = Act::None;
    let painter = ui.painter();
    super::hline(painter, 72.0, 0.0, screen.max.x);
    let back = Rect::from_min_size(pos2(24.0, 14.0), vec2(132.0, 44.0));
    let back_label = if view.back == View::Feed { "Feed" } else { "Your space" };
    if button(ui, back, back_label, ButtonStyle::Quiet).clicked() {
        act = Act::Back;
    }
    let painter = ui.painter();
    logo(painter, pos2(back.max.x + 26.0, 36.0), 18.0);
    painter.text(pos2(back.max.x + 46.0, 26.0), Align2::LEFT_CENTER, "Your profile", theme::ui_bold(18.0), c(theme::TEXT));
    let sub = match view.step {
        Step::Interview => "STEP 1 OF 2 · INTERVIEW",
        Step::Review => "STEP 2 OF 2 · REVIEW AND PUBLISH",
    };
    painter.text(pos2(back.max.x + 46.0, 48.0), Align2::LEFT_CENTER, sub, theme::mono(11.0), c(theme::TEXT_2));

    // The two steps, as in the builder.
    let labels = [(Step::Interview, "Interview", 118.0), (Step::Review, "Review", 102.0)];
    let total: f32 = labels.iter().map(|l| l.2).sum::<f32>() + 8.0;
    let mut x = screen.center().x - total / 2.0;
    for (i, (step, text, w)) in labels.iter().enumerate() {
        let r = Rect::from_min_size(pos2(x, 18.0), vec2(*w, 36.0));
        let on = view.step == *step;
        let resp = ui.interact(r, Id::new(("profile-step", i)), Sense::click());
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
            (i + 1).to_string(),
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
        if resp.clicked() && !on {
            view.step = *step;
            if *step == Step::Interview && view.question.is_none() && view.working.is_none() && !view.done {
                act = Act::Ask;
            }
        }
        x += w + 8.0;
    }

    let main = Rect::from_min_size(pos2(screen.max.x - 24.0 - 150.0, 14.0), vec2(150.0, 44.0));
    match view.step {
        Step::Interview => {
            if button(ui, main, "Review", ButtonStyle::Primary).clicked() {
                view.step = Step::Review;
            }
        }
        Step::Review => {
            let signed_in = core.ui.social.handle.is_some();
            if view.publishing {
                disabled(ui, main, "Publishing…");
            } else if !signed_in {
                disabled(ui, main, "Publish");
                if ui.interact(main, Id::new("publish-why"), Sense::hover()).hovered() {
                    egui::Tooltip::always_open(ui.ctx().clone(), ui.layer_id(), Id::new("publish-why-tip"), egui::PopupAnchor::Pointer)
                        .show(|ui| {
                            ui.label("Create your account first");
                        });
                }
            } else if button(ui, main, "Publish", ButtonStyle::Primary).clicked() {
                act = Act::Save(true);
            }
            let save = Rect::from_min_size(pos2(main.min.x - 12.0 - 96.0, 14.0), vec2(96.0, 44.0));
            if button(ui, save, "Save", ButtonStyle::Quiet).clicked() {
                act = Act::Save(false);
            }
        }
    }
    act
}

fn disabled(ui: &Ui, rect: Rect, text: &str) {
    let painter = ui.painter();
    painter.rect(rect, CornerRadius::same(10), c([0x1A, 0x1E, 0x28]), Stroke::NONE, StrokeKind::Inside);
    painter.text(rect.center(), Align2::CENTER_CENTER, text, theme::ui_bold(14.5), c([0x5B, 0x61, 0x70]));
}

fn interview_pane(ui: &mut Ui, core: &Core, view: &mut ProfileView, rect: Rect) -> Option<Act> {
    let mut act = None;
    let painter = ui.painter();
    super::panel(painter, rect, 18);
    painter.text(rect.min + vec2(20.0, 20.0), Align2::LEFT_TOP, "Tell your agent about you", theme::ui_bold(22.0), c(theme::TEXT));
    let sub = painter.layout(
        "One question at a time. Skip anything you do not want to share. You decide what others see in the next step.".into(),
        theme::ui(14.0),
        c(theme::TEXT_2),
        rect.width() - 40.0,
    );
    painter.galley(rect.min + vec2(20.0, 50.0), sub, c(theme::TEXT_2));
    super::hline(painter, rect.min.y + 92.0, rect.min.x + 20.0, rect.max.x - 20.0);

    let input_h = 96.0;
    let chips_h = 44.0;
    let chat =
        Rect::from_min_max(pos2(rect.min.x + 20.0, rect.min.y + 104.0), pos2(rect.max.x - 20.0, rect.max.y - input_h - chips_h - 16.0));
    let mut chat_ui = ui.new_child(egui::UiBuilder::new().max_rect(chat).layout(egui::Layout::top_down(egui::Align::Min)));
    egui::ScrollArea::vertical().id_salt("interview-chat").stick_to_bottom(true).auto_shrink([false, false]).show(&mut chat_ui, |ui| {
        let width = chat.width() - 14.0;
        let bubble = |ui: &mut Ui, role: Role, text: &str| {
            let m = crate::world::ChatMessage { id: String::new(), role, text: text.to_string(), done: true };
            super::agent::bubble(ui, &m, width);
            ui.add_space(8.0);
        };
        for t in &view.turns {
            bubble(ui, Role::Agent, &t.question);
            bubble(ui, Role::User, &t.answer);
        }
        if let Some(q) = &view.question {
            bubble(ui, Role::Agent, q);
        }
        if view.done {
            bubble(ui, Role::Agent, "That is everything I wanted to ask. Review your profile now and decide who sees what.");
        }
        if let Some(e) = &view.failed {
            bubble(ui, Role::System, e);
        }
        if let Some(w) = &view.working {
            let dots = ".".repeat(1 + (core.world.time * 2.0) as usize % 3);
            ui.label(egui::RichText::new(format!("{w}{dots}")).font(theme::mono(11.5)).color(c(theme::WORKING)));
        }
    });

    let busy = view.working.is_some();
    let can_answer = view.question.is_some() && !busy;
    let chip_y = rect.max.y - input_h - chips_h - 4.0;
    let mut x = rect.min.x + 20.0;
    let mut send: Option<String> = None;
    for (text, answer) in [("Skip this", "I would rather not say."), ("That is all", "That is all I want to share.")] {
        let w = ui.painter().layout_no_wrap(text.to_string(), theme::ui(13.0), c(theme::TEXT)).size().x + 26.0;
        let r = Rect::from_min_size(pos2(x, chip_y), vec2(w, 34.0));
        if can_answer && button(ui, r, text, ButtonStyle::Quiet).clicked() {
            send = Some(answer.to_string());
        }
        x += w + 8.0;
    }
    if view.failed.is_some() && !busy {
        let r = Rect::from_min_size(pos2(x, chip_y), vec2(110.0, 34.0));
        if button(ui, r, "Ask again", ButtonStyle::Quiet).clicked() {
            act = Some(Act::Ask);
        }
    }
    if view.done {
        let r = Rect::from_min_size(pos2(rect.max.x - 20.0 - 150.0, chip_y), vec2(150.0, 34.0));
        if button(ui, r, "Review now", ButtonStyle::Primary).clicked() {
            view.step = Step::Review;
        }
    }
    let input = Rect::from_min_max(pos2(rect.min.x + 20.0, rect.max.y - input_h), pos2(rect.max.x - 20.0, rect.max.y - 16.0));
    ui.painter().rect(input, CornerRadius::same(12), c([0x16, 0x1A, 0x23]), Stroke::new(1.0, c([0x2A, 0x30, 0x3D])), StrokeKind::Inside);
    let mut input_ui =
        ui.new_child(egui::UiBuilder::new().max_rect(Rect::from_min_max(input.min + vec2(14.0, 12.0), input.max - vec2(110.0, 10.0))));
    input_ui.add_enabled_ui(can_answer, |ui| {
        let hint = if view.done {
            "The interview is done"
        } else if busy {
            "Wait for the next question"
        } else {
            "Your answer"
        };
        let resp = ui.add(
            egui::TextEdit::multiline(&mut view.answer)
                .id(Id::new("interview-answer"))
                .frame(egui::Frame::NONE)
                .hint_text(egui::RichText::new(hint).color(c([0x6B, 0x72, 0x82])))
                .font(theme::ui(14.5))
                .desired_rows(3)
                .desired_width(input.width() - 130.0)
                .return_key(Some(egui::KeyboardShortcut::new(egui::Modifiers::SHIFT, egui::Key::Enter))),
        );
        if resp.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter) && !i.modifiers.shift) {
            send = Some(view.answer.trim().to_string());
        }
    });
    let send_r = Rect::from_min_size(pos2(input.max.x - 96.0, input.max.y - 52.0), vec2(84.0, 40.0));
    if can_answer && button(ui, send_r, "Send", ButtonStyle::Primary).clicked() {
        send = Some(view.answer.trim().to_string());
    }
    if let (Some(answer), Some(question)) = (send.filter(|a| !a.is_empty()), view.question.clone()) {
        view.turns.push(InterviewTurn { question, answer });
        view.question = None;
        view.answer.clear();
        act = Some(Act::Ask);
    }
    act
}

/// The profile as it grows, with who sees each part.
fn preview_pane(ui: &mut Ui, core: &Core, view: &ProfileView, rect: Rect) {
    let painter = ui.painter();
    super::panel(painter, rect, 18);
    painter.text(rect.min + vec2(20.0, 22.0), Align2::LEFT_TOP, "Your profile so far", theme::ui_bold(17.0), c(theme::TEXT));
    let who = core.ui.social.handle.as_ref().map_or_else(|| "NO ACCOUNT YET".to_string(), |h| format!("@{}", h.to_uppercase()));
    painter.text(pos2(rect.max.x - 20.0, rect.min.y + 26.0), Align2::RIGHT_TOP, who, theme::mono(11.0), c(theme::TEXT_2));
    let p = &view.draft;
    let x = rect.min.x + 24.0;
    let mut y = rect.min.y + 70.0;
    if p.is_empty() {
        let g = painter.layout(
            "Nothing yet. Your answers fill this in, and you can change everything before anyone sees it.".into(),
            theme::ui(15.0),
            c(theme::TEXT_2),
            rect.width() - 48.0,
        );
        painter.galley(pos2(x, y), g, c(theme::TEXT_2));
        return;
    }
    // Photos, then the name and the headline beside them.
    let size = 104.0;
    let mut px = x;
    for photo in p.photos.iter().take(3) {
        let r = Rect::from_min_size(pos2(px, y), vec2(size, size));
        picture(painter, &core.ui.social.pictures, Some(&photo_source(core, &view.agent, &photo.url)), "?", r);
        px += size + 10.0;
    }
    let tx = if p.photos.is_empty() { x } else { px + 10.0 };
    let tw = rect.max.x - 24.0 - tx;
    let mut ty = y;
    if let Some(name) = &p.name {
        let r = painter.text(pos2(tx, ty), Align2::LEFT_TOP, &name.value, theme::ui_bold(26.0), c(theme::TEXT));
        vis_pill(painter, pos2(r.max.x + 12.0, r.center().y), name.visibility);
        ty = r.max.y + 6.0;
    }
    if let Some(h) = &p.headline {
        let g = painter.layout(h.value.clone(), theme::ui(16.0), c([0xC9, 0xCE, 0xD9]), tw - 90.0);
        let gh = g.size();
        painter.galley(pos2(tx, ty), g, c(theme::TEXT));
        vis_pill(painter, pos2(tx + gh.x + 12.0, ty + 11.0), h.visibility);
        ty += gh.y + 8.0;
    }
    y = ty.max(if p.photos.is_empty() { y } else { y + size }) + 22.0;
    super::hline(painter, y - 10.0, x, rect.max.x - 24.0);
    for (what, value, vis) in facts(p) {
        if y > rect.max.y - 30.0 {
            break;
        }
        painter.text(pos2(x, y), Align2::LEFT_TOP, what, theme::mono(11.0), c(theme::TEXT_2));
        let v = painter.text(
            pos2(x + 100.0, y - 2.0),
            Align2::LEFT_TOP,
            super::fit(painter, &value, theme::ui(15.0), rect.width() - 260.0),
            theme::ui(15.0),
            c(theme::TEXT),
        );
        vis_pill(painter, pos2(v.max.x + 10.0, v.center().y), vis);
        y += 30.0;
    }
}

fn vis_pill(painter: &egui::Painter, left_center: egui::Pos2, v: Visibility) {
    let (text, color) = match v {
        Visibility::Public => ("PUBLIC", theme::NEEDS_YOU),
        Visibility::Friends => ("FRIENDS", MINT),
        Visibility::Private => ("PRIVATE", theme::TEXT_2),
    };
    small_pill(painter, left_center, text, color);
}

/// The form with every field and its visibility.
fn review_pane(ui: &mut Ui, core: &mut Core, view: &mut ProfileView, body: Rect) -> Option<Act> {
    let mut act = None;
    let panel = Rect::from_center_size(pos2(body.center().x, body.center().y), vec2(body.width().min(920.0), body.height()));
    super::panel(ui.painter(), panel, 18);
    let inner = panel.shrink2(vec2(28.0, 24.0));
    let mut form = ui.new_child(egui::UiBuilder::new().max_rect(inner).layout(egui::Layout::top_down(egui::Align::Min)));
    form.spacing_mut().item_spacing = vec2(10.0, 8.0);
    let before = view.draft.clone();
    egui::ScrollArea::vertical().id_salt("profile-form").auto_shrink([false, false]).show(&mut form, |ui| {
        let width = inner.width() - 16.0;
        ui.label(egui::RichText::new("Review and publish").font(theme::ui_bold(22.0)).color(c(theme::TEXT)));
        ui.label(
            egui::RichText::new("Public is for everyone, friends only for people you befriend, private never leaves this computer.")
                .font(theme::ui(14.0))
                .color(c(theme::TEXT_2)),
        );
        ui.add_space(6.0);
        if let Some(a) = account_box(ui, core, view, width) {
            act = Some(a);
        }
        if let Some(e) = &view.error {
            ui.label(egui::RichText::new(e).font(theme::mono(12.0)).color(c(RED)));
        }
        heading(ui, "NAME AND HEADLINE");
        text_field(ui, &mut view.draft.name, "Your name", width, "name");
        text_field(ui, &mut view.draft.headline, "One line about you", width, "headline");

        heading(ui, "PHOTOS");
        photos(ui, core, view, width);

        heading(ui, "SCHOOLS AND UNIVERSITY");
        let mut remove = None;
        for (i, e) in view.draft.education.iter_mut().enumerate() {
            ui.horizontal(|ui| {
                field(ui, &mut e.school, "School", width * 0.25);
                optional(ui, &mut e.degree, "Degree", width * 0.2);
                year(ui, &mut e.from, ("edu-from", i));
                year(ui, &mut e.to, ("edu-to", i));
                vis(ui, &mut e.visibility, ("edu-vis", i));
                if remove_button(ui) {
                    remove = Some(i);
                }
            });
        }
        if let Some(i) = remove {
            view.draft.education.remove(i);
        }
        if add_button(ui, "Add a school") {
            view.draft.education.push(Education {
                school: String::new(),
                degree: None,
                from: None,
                to: None,
                visibility: Visibility::Friends,
            });
        }

        heading(ui, "WORK");
        let mut remove = None;
        for (i, w) in view.draft.work.iter_mut().enumerate() {
            ui.horizontal(|ui| {
                field(ui, &mut w.org, "Organisation", width * 0.25);
                optional(ui, &mut w.role, "Role", width * 0.2);
                year(ui, &mut w.from, ("work-from", i));
                year(ui, &mut w.to, ("work-to", i));
                vis(ui, &mut w.visibility, ("work-vis", i));
                if remove_button(ui) {
                    remove = Some(i);
                }
            });
        }
        if let Some(i) = remove {
            view.draft.work.remove(i);
        }
        if add_button(ui, "Add work") {
            view.draft.work.push(Work { org: String::new(), role: None, from: None, to: None, visibility: Visibility::Public });
        }

        heading(ui, "PLACES");
        list(ui, &mut view.draft.places, "City or region", width, "place", Visibility::Friends);
        heading(ui, "INTERESTS");
        list(ui, &mut view.draft.interests, "Something you like", width, "interest", Visibility::Public);

        heading(ui, "LINKS");
        let mut remove = None;
        for (i, l) in view.draft.links.iter_mut().enumerate() {
            ui.horizontal(|ui| {
                field(ui, &mut l.url, "https://", width * 0.6);
                vis(ui, &mut l.visibility, ("link-vis", i));
                if remove_button(ui) {
                    remove = Some(i);
                }
            });
        }
        if let Some(i) = remove {
            view.draft.links.remove(i);
        }
        if add_button(ui, "Add a link") {
            view.draft.links.push(ProfileLink { url: String::new(), visibility: Visibility::Public });
        }
        ui.add_space(12.0);
        let note = match (&core.ui.social.handle, core.ui.social.published) {
            (Some(_), Some(at)) => format!("Published {}. Publish again after a change.", ago(at).to_lowercase()),
            (Some(_), None) => {
                "Not published yet. Publish puts the public and friends fields on the index and lists you in the search.".into()
            }
            (None, _) => "Saved on this computer only.".into(),
        };
        ui.label(egui::RichText::new(note).font(theme::ui(13.0)).color(ca(theme::TEXT_2, 210)));
    });
    if view.draft != before {
        view.dirty = true;
    }
    act
}

fn heading(ui: &mut Ui, text: &str) {
    ui.add_space(8.0);
    ui.label(egui::RichText::new(text).font(theme::mono(11.0)).color(c(theme::TEXT_2)));
}

fn account_box(ui: &mut Ui, core: &Core, view: &mut ProfileView, width: f32) -> Option<Act> {
    let mut act = None;
    let host = core.ui.social.index.split("://").nth(1).unwrap_or(&core.ui.social.index).trim_end_matches('/').to_string();
    egui::Frame::new().fill(c([0x16, 0x1A, 0x23])).corner_radius(CornerRadius::same(12)).inner_margin(egui::Margin::same(14)).show(
        ui,
        |ui| {
            ui.set_width(width - 28.0);
            match &core.ui.social.handle {
                Some(h) => {
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(format!("@{h}")).font(theme::ui_semibold(16.0)).color(c(theme::TEXT)));
                        ui.label(
                            egui::RichText::new(format!("your account on the index at {host}"))
                                .font(theme::ui(14.0))
                                .color(c(theme::TEXT_2)),
                        );
                    });
                }
                None => {
                    ui.label(egui::RichText::new("Create your account on the index").font(theme::ui_semibold(16.0)).color(c(theme::TEXT)));
                    ui.label(
                        egui::RichText::new(format!(
                            "Pick a handle for {host}. People find you by your name, the handle is your address there."
                        ))
                        .font(theme::ui(13.5))
                        .color(c(theme::TEXT_2)),
                    );
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("@").font(theme::ui(15.0)).color(c(theme::TEXT_2)));
                        ui.add(egui::TextEdit::singleline(&mut view.handle).hint_text("handle").desired_width(220.0));
                        let ok = !view.handle.trim().is_empty() && !view.creating_account;
                        if ui
                            .add_enabled(ok, egui::Button::new(if view.creating_account { "Creating…" } else { "Create account" }))
                            .clicked()
                        {
                            act = Some(Act::CreateAccount(view.handle.trim().to_lowercase()));
                        }
                    });
                    if let Some(e) = &core.ui.social.account_error {
                        ui.label(egui::RichText::new(e).font(theme::mono(12.0)).color(c(RED)));
                    }
                }
            }
        },
    );
    act
}

/// A single line text field as tall as the visibility switch.
fn field(ui: &mut Ui, text: &mut String, hint: &str, width: f32) -> egui::Response {
    ui.add(
        egui::TextEdit::singleline(text)
            .hint_text(egui::RichText::new(hint).color(c([0x6B, 0x72, 0x82])))
            .font(theme::ui(14.5))
            .margin(egui::Margin::symmetric(8, 6))
            .desired_width(width),
    )
}

fn remove_button(ui: &mut Ui) -> bool {
    ui.add(egui::Button::new(egui::RichText::new("Remove").font(theme::ui(13.0)).color(c(theme::TEXT_2))).frame(false)).clicked()
}

fn add_button(ui: &mut Ui, text: &str) -> bool {
    ui.add(egui::Button::new(egui::RichText::new(format!("+  {text}")).font(theme::ui(13.5)).color(c([0xC9, 0xCE, 0xD9])))).clicked()
}

fn text_field(ui: &mut Ui, slot: &mut Option<ProfileText>, hint: &str, width: f32, id: &str) {
    ui.horizontal(|ui| {
        let mut value = slot.as_ref().map(|f| f.value.clone()).unwrap_or_default();
        let mut visibility = slot.as_ref().map_or(Visibility::Public, |f| f.visibility);
        field(ui, &mut value, hint, width * 0.62);
        vis(ui, &mut visibility, (id, 0));
        *slot = (!value.trim().is_empty()).then_some(ProfileText { value, visibility });
    });
}

fn optional(ui: &mut Ui, value: &mut Option<String>, hint: &str, width: f32) {
    let mut text = value.clone().unwrap_or_default();
    field(ui, &mut text, hint, width);
    *value = (!text.trim().is_empty()).then_some(text);
}

/// A year as text, kept in egui's memory while it is typed.
fn year(ui: &mut Ui, value: &mut Option<i32>, id: impl egui::AsId) {
    let id = Id::new(id);
    let mut text = ui.data_mut(|d| d.get_temp::<String>(id)).unwrap_or_else(|| value.map(|y| y.to_string()).unwrap_or_default());
    let resp = field(ui, &mut text, "year", 60.0);
    if resp.changed() {
        *value = text.trim().parse().ok();
    }
    if resp.has_focus() {
        ui.data_mut(|d| d.insert_temp(id, text));
    } else {
        ui.data_mut(|d| d.remove::<String>(id));
    }
}

fn vis(ui: &mut Ui, value: &mut Visibility, id: impl egui::AsId) {
    let (rect, _) = ui.allocate_exact_size(vec2(204.0, 30.0), Sense::hover());
    if let Some(v) = visibility_switch(ui, rect, *value, id, true) {
        *value = v;
    }
}

fn list(ui: &mut Ui, items: &mut Vec<ProfileText>, hint: &str, width: f32, id: &str, default: Visibility) {
    let mut remove = None;
    for (i, item) in items.iter_mut().enumerate() {
        ui.horizontal(|ui| {
            field(ui, &mut item.value, hint, width * 0.45);
            vis(ui, &mut item.visibility, (id, i));
            if remove_button(ui) {
                remove = Some(i);
            }
        });
    }
    if let Some(i) = remove {
        items.remove(i);
    }
    if add_button(ui, if id == "interest" { "Add an interest" } else { "Add a place" }) {
        items.push(ProfileText { value: String::new(), visibility: default });
    }
}

fn photos(ui: &mut Ui, core: &mut Core, view: &mut ProfileView, width: f32) {
    let size = 112.0;
    let mut remove = None;
    if !view.draft.photos.is_empty() {
        let (row, _) = ui.allocate_exact_size(vec2(width, size + 44.0), Sense::hover());
        let mut x = row.min.x;
        let agent = view.agent.clone();
        for (i, photo) in view.draft.photos.iter_mut().enumerate() {
            let r = Rect::from_min_size(pos2(x, row.min.y), vec2(size, size));
            picture(ui.painter(), &core.ui.social.pictures, Some(&photo_source(core, &agent, &photo.url)), "?", r);
            let close = Rect::from_min_size(pos2(r.max.x - 26.0, r.min.y + 4.0), vec2(22.0, 22.0));
            let resp = ui.interact(close, Id::new(("photo-remove", i)), Sense::click());
            ui.painter().circle_filled(close.center(), 11.0, ca([0x0B, 0x0D, 0x12], 220));
            ui.painter().text(close.center(), Align2::CENTER_CENTER, "x", theme::ui_bold(13.0), c(theme::TEXT));
            if resp.clicked() {
                remove = Some(i);
            }
            let seg = Rect::from_min_size(pos2(r.min.x, r.max.y + 8.0), vec2(size, 28.0));
            if let Some(v) = visibility_switch(ui, seg, photo.visibility, ("photo-vis", i), false) {
                photo.visibility = v;
            }
            x += size + 14.0;
            if x + size > row.max.x {
                break;
            }
        }
    }
    if let Some(i) = remove {
        view.draft.photos.remove(i);
    }
    ui.horizontal(|ui| {
        field(ui, &mut view.photo_path, "/path/to/photo.jpg", width * 0.45);
        if add_button(ui, "Add photo") && !view.photo_path.trim().is_empty() {
            let path = PathBuf::from(view.photo_path.trim());
            add_photo(core, view, &path);
        }
        ui.label(egui::RichText::new("or drop PNG or JPEG files on this window").font(theme::ui(13.0)).color(c(theme::TEXT_2)));
    });
}

/// Copies a photo into the personal agent's `photos/` and adds it, visible to friends.
fn add_photo(core: &mut Core, view: &mut ProfileView, path: &Path) {
    let result = (|| -> Result<String, String> {
        let dir = core.agent_dir(&view.agent).ok_or("the personal agent is gone")?.path.clone();
        let size = std::fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?.len();
        if size > PHOTO_MAX {
            return Err(format!("{} is larger than 2 MB", path.display()));
        }
        let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let ext = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
            "png"
        } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
            "jpg"
        } else {
            return Err(format!("{} is not a PNG or JPEG photo", path.display()));
        };
        // The personal agent writes its directory, so a photo goes only into a
        // free name and never through a symlink there (safefs).
        let mut n = view.draft.photos.len() + 1;
        loop {
            let name = format!("photos/photo-{n}.{ext}");
            if meerora_protocol::safefs::create_new(&dir, &name, &bytes).map_err(|e| e.to_string())? {
                return Ok(name);
            }
            n += 1;
        }
    })();
    match result {
        Ok(url) => {
            view.draft.photos.push(ProfilePhoto { url, visibility: Visibility::Friends });
            view.photo_path.clear();
            view.dirty = true;
        }
        Err(e) => core.ui.toast(core.world.time, e, true),
    }
}
