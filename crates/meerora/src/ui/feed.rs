//! The feed (D27, D28): posts of friends and of the people and
//! organisations the user follows, newest first, with the news agent's
//! digest pinned on top. Beside it everything that waits for the user: post
//! drafts of the personal agent (D26), friend requests and the news
//! sources. No screen is designed for this yet (Q14), so it follows the
//! builder (screen 3) and the search: the feed on the left, the rest on
//! the right.
//!
//! It opens from the Feed button in the space header. A noticeboard device
//! in the space will open it too once devices exist (D7). Friends and
//! followed agents appear here only, never as figures in the space (Q12).
//!
//! This module also keeps what the daemon says about the account, the
//! profile, the drafts, the digest and the relations, and draws the Follow
//! and Befriend buttons and the profile of a person in the search.
//!
//! Text shown here never cites decision numbers, the user does not know them.

use std::collections::HashMap;
use std::time::Duration;

use egui::{Align2, Color32, CornerRadius, Id, LayerId, Order, Rect, Sense, Stroke, StrokeKind, Ui, pos2, vec2};
use meerora_protocol::{
    AgentRole, Command, Digest, Event, IndexEntry, ManifestKind, Post, PostDraft, Profile, Reaction, Relation, Relations, SocialAction,
    Visibility,
};

use super::index::{MINT, Pictures, picture, small_pill};
use super::{ButtonStyle, View, button, fit, label, logo};
use crate::engine::Core;
use crate::theme::{self, c, ca};

const RED: [u8; 3] = [0xE0, 0x67, 0x5A];

/// What the daemon told the app about the social side, plus the feed screen.
pub struct Social {
    pub index: String,
    pub handle: Option<String>,
    pub account_error: Option<String>,
    pub own_profile: Option<Profile>,
    pub published: Option<u64>,
    pub profile_error: Option<String>,
    pub drafts: Vec<PostDraft>,
    pub digest: Option<Digest>,
    pub relations: Option<Relations>,
    pub posts: Vec<Post>,
    pub next: Option<String>,
    pub feed_error: Option<String>,
    /// Request ids in flight. `older` marks a request for older posts.
    feed_request: Option<String>,
    older: bool,
    social_request: Option<String>,
    /// Draft texts while the user edits them, by draft id.
    edits: HashMap<String, (String, Visibility)>,
    /// Drafts and actions sent but not answered yet.
    sending: Vec<String>,
    /// The news agent's `config/sources.txt` while it is edited.
    sources: Option<String>,
    sources_saved: bool,
    template_pending: bool,
    pub pictures: Pictures,
    /// The profile screen, while it is open.
    pub profile: Option<super::profile::ProfileView>,
    seq: u64,
}

impl Default for Social {
    fn default() -> Self {
        Self {
            index: meerora_protocol::DEFAULT_INDEX_URL.into(),
            handle: None,
            account_error: None,
            own_profile: None,
            published: None,
            profile_error: None,
            drafts: Vec::new(),
            digest: None,
            relations: None,
            posts: Vec::new(),
            next: None,
            feed_error: None,
            feed_request: None,
            older: false,
            social_request: None,
            edits: HashMap::new(),
            sending: Vec::new(),
            sources: None,
            sources_saved: false,
            template_pending: false,
            pictures: Pictures::new(),
            profile: None,
            seq: 0,
        }
    }
}

impl Social {
    fn request_id(&mut self, what: &str) -> String {
        self.seq += 1;
        format!("{what}-{}", self.seq)
    }

    /// Requests, drafts and nothing else that needs the user.
    pub fn waiting(&self) -> usize {
        self.drafts.len() + self.relations.as_ref().map_or(0, |r| r.requests.len())
    }
}

/// Sends a command, or says why it cannot go.
fn send(core: &mut Core, command: Command) -> bool {
    match &core.link {
        Some(link) if core.ui.daemon_connected => {
            link.send(&command);
            true
        }
        _ => {
            core.ui.toast(core.world.time, "The agent daemon is not running.", true);
            false
        }
    }
}

/// The personal agent's id, if the user has one.
pub fn personal_agent(core: &Core) -> Option<String> {
    agent_with(core, AgentRole::Personal)
}

pub fn news_agent(core: &Core) -> Option<String> {
    agent_with(core, AgentRole::News)
}

fn agent_with(core: &Core, role: AgentRole) -> Option<String> {
    core.world
        .agents
        .iter()
        .map(|a| a.id.as_str())
        .find(|id| core.agent_dir(id).is_some_and(|d| d.config.role == Some(role)))
        .map(String::from)
}

/// Asks the daemon for the relations, for the buttons in the search and the feed.
pub fn refresh_relations(core: &mut Core) {
    if core.ui.social.handle.is_none() || !core.ui.daemon_connected {
        return;
    }
    let request = core.ui.social.request_id("relations");
    core.ui.social.social_request = Some(request.clone());
    send(core, Command::Social { request, action: SocialAction::Refresh });
}

fn social_action(core: &mut Core, action: SocialAction) {
    let request = core.ui.social.request_id("social");
    core.ui.social.social_request = Some(request.clone());
    send(core, Command::Social { request, action });
}

fn read_feed(core: &mut Core, before: Option<String>) {
    if core.ui.social.handle.is_none() {
        return;
    }
    let request = core.ui.social.request_id("feed");
    core.ui.social.feed_request = Some(request.clone());
    core.ui.social.older = before.is_some();
    if before.is_none() {
        core.ui.social.feed_error = None;
    }
    send(core, Command::ReadFeed { request, before });
}

/// Opens the feed and reads it.
pub fn open(core: &mut Core) {
    core.ui.view = View::Feed;
    core.ui.social.sources = None;
    read_feed(core, None);
    refresh_relations(core);
}

/// Opens the builder for the user's personal agent.
pub fn setup_personal(core: &mut Core) {
    super::open_builder(core);
    if let Some(b) = core.ui.builder.as_mut() {
        b.role = Some(AgentRole::Personal);
        b.messages.push((
            meerora_protocol::Role::System,
            "This agent represents you in the meerora index. Describe how it should look and give it a name. An interview fills your profile afterwards.".into(),
        ));
    }
}

/// Takes the daemon's social events. Returns true for the events this module owns.
pub fn on_event(core: &mut Core, event: &Event) -> bool {
    let now = core.world.time;
    match event {
        Event::Account { index, handle, error } => {
            let s = &mut core.ui.social;
            s.index = index.clone();
            if error.is_none() || handle.is_some() {
                s.handle = handle.clone();
            }
            s.account_error = error.clone();
            match error {
                Some(e) => core.ui.toast(now, e.clone(), true),
                None if handle.is_some() => refresh_relations(core),
                None => {}
            }
        }
        Event::InterviewStep { .. } => super::profile::on_step(core, event),
        Event::OwnProfile { profile, published, error } => {
            let s = &mut core.ui.social;
            let newly = published.is_some() && *published != s.published;
            s.own_profile = profile.as_deref().cloned();
            s.published = *published;
            s.profile_error = error.clone();
            super::profile::on_own_profile(core, newly, error.clone());
        }
        Event::Social { request, relations, error } => {
            let s = &mut core.ui.social;
            if s.social_request.as_ref().is_none_or(|r| r == request) {
                s.social_request = None;
            }
            if let Some(r) = relations {
                for url in r.requests.iter().chain(&r.follows).filter_map(|r| r.actor.picture.as_ref()) {
                    s.pictures.request(url);
                }
                s.relations = Some(r.clone());
            }
            if let Some(e) = error {
                core.ui.toast(now, e.clone(), true);
            }
        }
        Event::Feed { request, posts, next, error } => {
            let s = &mut core.ui.social;
            let mine = s.feed_request.as_ref() == Some(request);
            if !(request.is_empty() || mine || s.feed_request.is_none()) {
                return true;
            }
            let older = mine && s.older;
            if mine {
                s.feed_request = None;
                s.older = false;
            }
            s.feed_error = error.clone();
            if older {
                s.posts.extend(posts.iter().cloned());
            } else if error.is_none() {
                s.posts = posts.clone();
            }
            s.next = next.clone();
            for url in posts.iter().filter_map(|p| p.author.picture.as_ref()) {
                s.pictures.request(url);
            }
        }
        Event::PostDrafts { drafts, published } => {
            let s = &mut core.ui.social;
            s.drafts = drafts.clone();
            s.edits.retain(|id, _| drafts.iter().any(|d| d.id == *id));
            s.sending.retain(|id| drafts.iter().any(|d| d.id == *id && d.error.is_none()));
            if let Some(post) = published {
                core.ui.toast(now, format!("Your post went out: {}", short(&post.text, 60)), false);
            }
        }
        Event::NewsDigest { digest } => core.ui.social.digest = digest.clone(),
        Event::TemplateAdded { agent, error, .. } => {
            core.ui.social.template_pending = false;
            match (agent, error) {
                (Some(id), _) => match meerora_protocol::load_agent(&core.assets.data_dir.join("agents").join(id)) {
                    Ok(dir) => {
                        let name = dir.config.name.clone();
                        core.adopt_agent(dir);
                        core.ui.social.sources = None;
                        core.ui.toast(now, format!("{name} moved into your space and writes your digest every morning"), false);
                    }
                    Err(e) => core.ui.toast(now, e, true),
                },
                (None, Some(e)) => core.ui.toast(now, e.clone(), true),
                _ => {}
            }
        }
        _ => return false,
    }
    true
}

/// At most `max` characters, with an ellipsis when cut.
pub fn short(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let cut: String = text.chars().take(max.saturating_sub(1)).collect();
    format!("{}…", cut.trim_end())
}

/// The Feed button left of the daemon chip in the space header, with the
/// number of things that wait. A click opens the feed.
pub fn header_button(ui: &mut Ui, core: &mut Core, screen: Rect) {
    let chip_text = if core.ui.daemon_connected { "AGENT DAEMON CONNECTED" } else { "AGENT DAEMON NOT RUNNING" };
    let chip_w = ui.painter().layout_no_wrap(chip_text.into(), theme::mono(11.0), Color32::WHITE).size().x + 34.0;
    let rect =
        Rect::from_min_size(pos2(screen.max.x - 24.0 - chip_w - 12.0 - 104.0, super::space::HEADER_H / 2.0 - 16.0), vec2(104.0, 32.0));
    let waiting = core.ui.social.waiting();
    let mut open_feed = false;
    egui::Area::new(Id::new("feed-button")).fade_in(false).fixed_pos(rect.min).order(Order::Foreground).show(ui.ctx(), |ui| {
        ui.allocate_exact_size(rect.size(), Sense::hover());
        let resp = ui.interact(rect, Id::new("feed-button-click"), Sense::click());
        let painter = ui.painter();
        let fill = if resp.hovered() { c([0x1A, 0x1F, 0x2A]) } else { c(theme::PANEL) };
        painter.rect(rect, CornerRadius::same(16), fill, Stroke::new(1.0, c(theme::BORDER)), StrokeKind::Inside);
        // A small pinboard.
        let board = Rect::from_center_size(pos2(rect.min.x + 22.0, rect.center().y), vec2(14.0, 11.0));
        painter.rect_stroke(board, CornerRadius::same(2), Stroke::new(1.3, c(theme::TEXT_2)), StrokeKind::Inside);
        painter.line_segment(
            [board.left_center() + vec2(3.0, -1.5), board.right_center() + vec2(-3.0, -1.5)],
            Stroke::new(1.2, c(theme::TEXT_2)),
        );
        painter.line_segment(
            [board.left_center() + vec2(3.0, 2.0), board.right_center() + vec2(-6.0, 2.0)],
            Stroke::new(1.2, c(theme::TEXT_2)),
        );
        painter.text(pos2(rect.min.x + 36.0, rect.center().y), Align2::LEFT_CENTER, "Feed", theme::ui(14.0), c(theme::TEXT));
        if waiting > 0 {
            let dot = pos2(rect.max.x - 20.0, rect.center().y);
            painter.circle_filled(dot, 10.0, c(theme::NEEDS_YOU));
            painter.text(dot, Align2::CENTER_CENTER, waiting.min(99).to_string(), theme::mono(10.5), c([0x0B, 0x1A, 0x26]));
        }
        if resp.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        open_feed = resp.clicked();
    });
    if open_feed {
        open(core);
    }
}

/// What the user clicked, done after drawing.
enum Act {
    Back,
    Refresh,
    Profile,
    SetupPersonal,
    Older,
    Approve(String),
    Discard(String),
    Edit(String),
    Accept(String),
    Decline(String),
    Unfollow(String),
    React(String, String, Reaction),
    Open(String, String, String),
    AddNews,
    SaveSources,
}

pub fn draw(ui: &mut Ui, core: &mut Core) {
    let mut state = std::mem::take(&mut core.ui.social);
    state.pictures.poll(ui.ctx());
    let screen = ui.ctx().content_rect();
    ui.ctx().layer_painter(LayerId::background()).rect_filled(screen, CornerRadius::ZERO, c(theme::BG));
    let mut acts = Vec::new();
    if state.sources.is_none()
        && let Some(id) = news_agent(core)
        && let Some(dir) = core.agent_dir(&id)
    {
        state.sources = Some(std::fs::read_to_string(dir.path.join("config/sources.txt")).unwrap_or_default());
    }
    egui::Area::new(Id::new("feed")).fade_in(false).fixed_pos(screen.min).order(Order::Middle).show(ui.ctx(), |ui| {
        ui.allocate_exact_size(screen.size(), Sense::hover());
        header(ui, core, &state, screen, &mut acts);
        let body = Rect::from_min_max(pos2(24.0, 92.0), pos2(screen.max.x - 24.0, screen.max.y - 24.0));
        let left = Rect::from_min_size(body.min, vec2((body.width() * 0.56).min(780.0), body.height()));
        let right = Rect::from_min_max(pos2(left.max.x + 16.0, body.min.y), body.max);
        feed_pane(ui, core, &mut state, left, &mut acts);
        side_pane(ui, core, &mut state, right, &mut acts);
    });
    if ui.input(|i| i.key_pressed(egui::Key::Escape)) && !ui.ctx().memory(|m| m.focused().is_some()) {
        acts.push(Act::Back);
    }
    if state.feed_request.is_some() || state.social_request.is_some() || !state.sending.is_empty() {
        ui.ctx().request_repaint_after(Duration::from_millis(100));
    }
    core.ui.social = state;
    for act in acts {
        run(core, act);
    }
}

fn run(core: &mut Core, act: Act) {
    match act {
        Act::Back => core.ui.view = View::Space,
        Act::Refresh => {
            read_feed(core, None);
            refresh_relations(core);
        }
        Act::Profile => match personal_agent(core) {
            Some(id) => super::profile::open_for(core, &id),
            None => setup_personal(core),
        },
        Act::SetupPersonal => setup_personal(core),
        Act::Older => {
            let next = core.ui.social.next.clone();
            read_feed(core, next);
        }
        Act::Approve(id) => {
            let (text, visibility) = core.ui.social.edits.remove(&id).map_or((None, None), |(t, v)| (Some(t), Some(v)));
            if send(core, Command::DecidePost { id: id.clone(), approve: true, text, visibility }) {
                core.ui.social.sending.push(id);
            }
        }
        Act::Discard(id) => {
            send(core, Command::DecidePost { id, approve: false, text: None, visibility: None });
        }
        Act::Edit(id) => {
            let s = &mut core.ui.social;
            if s.edits.contains_key(&id) {
                // Done editing keeps the text for Approve.
            } else if let Some(d) = s.drafts.iter().find(|d| d.id == id) {
                s.edits.insert(id, (d.text.clone(), d.visibility));
            }
        }
        Act::Accept(h) => social_action(core, SocialAction::Accept { handle: h }),
        Act::Decline(h) => social_action(core, SocialAction::Decline { handle: h }),
        Act::Unfollow(a) => social_action(core, SocialAction::Unfollow { actor: a }),
        Act::React(digest, item, reaction) => {
            send(core, Command::React { digest, item, reaction });
        }
        Act::Open(url, digest, item) => {
            if let Err(e) = std::process::Command::new("xdg-open").arg(&url).spawn() {
                core.ui.toast(core.world.time, format!("Cannot open {url}: {e}"), true);
            }
            send(core, Command::React { digest, item, reaction: Reaction::Opened });
        }
        Act::AddNews => {
            if send(core, Command::AddTemplate { template: "news".into() }) {
                core.ui.social.template_pending = true;
            }
        }
        Act::SaveSources => {
            let Some(id) = news_agent(core) else { return };
            let Some(dir) = core.agent_dir(&id).map(|d| d.path.clone()) else { return };
            let text = core.ui.social.sources.clone().unwrap_or_default();
            let path = dir.join("config/sources.txt");
            // The news agent writes its directory, so no symlink there is followed (safefs).
            match meerora_protocol::safefs::replace(&dir, "config/sources.txt", text.as_bytes()) {
                Ok(()) => {
                    core.ui.social.sources_saved = true;
                    core.ui.toast(core.world.time, "Saved the news sources", false);
                }
                Err(e) => core.ui.toast(core.world.time, format!("{}: {e}", path.display()), true),
            }
        }
    }
}

fn header(ui: &mut Ui, core: &Core, state: &Social, screen: Rect, acts: &mut Vec<Act>) {
    let painter = ui.painter();
    super::hline(painter, 72.0, 0.0, screen.max.x);
    let back = Rect::from_min_size(pos2(24.0, 14.0), vec2(132.0, 44.0));
    if button(ui, back, "Your space", ButtonStyle::Quiet).clicked() {
        acts.push(Act::Back);
    }
    let painter = ui.painter();
    logo(painter, pos2(back.max.x + 26.0, 36.0), 18.0);
    painter.text(pos2(back.max.x + 46.0, 26.0), Align2::LEFT_CENTER, "Feed", theme::ui_bold(18.0), c(theme::TEXT));
    let host = state.index.split("://").nth(1).unwrap_or(&state.index).trim_end_matches('/').to_uppercase();
    let who = state.handle.as_ref().map_or_else(|| "NO ACCOUNT YET".to_string(), |h| format!("@{}", h.to_uppercase()));
    painter.text(
        pos2(back.max.x + 46.0, 48.0),
        Align2::LEFT_CENTER,
        format!("MEERORA INDEX · {host} · {who}"),
        theme::mono(11.0),
        c(theme::TEXT_2),
    );

    let main = Rect::from_min_size(pos2(screen.max.x - 24.0 - 220.0, 14.0), vec2(220.0, 44.0));
    let (text, act) =
        if personal_agent(core).is_some() { ("Your profile", Act::Profile) } else { ("Set up your personal agent", Act::SetupPersonal) };
    if button(ui, main, text, ButtonStyle::Primary).clicked() {
        acts.push(act);
    }
    let refresh = Rect::from_min_size(pos2(main.min.x - 12.0 - 104.0, 14.0), vec2(104.0, 44.0));
    if state.handle.is_some() && button(ui, refresh, "Refresh", ButtonStyle::Quiet).clicked() {
        acts.push(Act::Refresh);
    }
}

fn feed_pane(ui: &mut Ui, core: &Core, state: &mut Social, rect: Rect, acts: &mut Vec<Act>) {
    let painter = ui.painter();
    super::panel(painter, rect, 18);
    painter.text(rect.min + vec2(20.0, 20.0), Align2::LEFT_TOP, "Your feed", theme::ui_bold(22.0), c(theme::TEXT));
    let status = if let Some(e) = &state.feed_error {
        (e.clone(), RED)
    } else if state.feed_request.is_some() {
        ("READING THE FEED…".to_string(), theme::WORKING)
    } else if state.handle.is_none() {
        ("NO ACCOUNT YET".to_string(), theme::TEXT_2)
    } else {
        (format!("{} POSTS · FRIENDS AND WHO YOU FOLLOW · NEWEST FIRST", state.posts.len()), theme::TEXT_2)
    };
    let galley = painter.layout(status.0, theme::mono(11.0), c(status.1), rect.width() - 40.0);
    let status_h = galley.size().y;
    painter.galley(pos2(rect.min.x + 20.0, rect.min.y + 54.0), galley, c(status.1));

    let list = Rect::from_min_max(pos2(rect.min.x + 12.0, rect.min.y + 66.0 + status_h), pos2(rect.max.x - 12.0, rect.max.y - 12.0));
    let mut list_ui = ui.new_child(egui::UiBuilder::new().max_rect(list).layout(egui::Layout::top_down(egui::Align::Min)));
    egui::ScrollArea::vertical().id_salt("feed-posts").auto_shrink([false, false]).show(&mut list_ui, |ui| {
        let width = list.width() - 14.0;
        digest_card(ui, core, state, width, acts);
        ui.add_space(10.0);
        if state.handle.is_none() {
            empty_note(ui, width, "You have no account on this index yet. Set up your personal agent and publish its profile, then follow people and organisations from the search.");
        } else if state.posts.is_empty() && state.feed_request.is_none() && state.feed_error.is_none() {
            empty_note(ui, width, "Nothing here yet. Find people and organisations with Get an external agent and follow them, or befriend someone.");
        }
        let my = state.handle.clone().unwrap_or_default();
        for post in &state.posts {
            post_row(ui, &state.pictures, post, &my, width);
            ui.add_space(8.0);
        }
        if state.next.is_some() {
            let (r, _) = ui.allocate_exact_size(vec2(width, 40.0), Sense::hover());
            let b = Rect::from_center_size(r.center(), vec2(180.0, 36.0));
            if button(ui, b, "Older posts", ButtonStyle::Quiet).clicked() {
                acts.push(Act::Older);
            }
        }
    });
}

fn empty_note(ui: &mut Ui, width: f32, text: &str) {
    let galley = ui.painter().layout(text.into(), theme::ui(14.5), c(theme::TEXT_2), width - 40.0);
    let (r, _) = ui.allocate_exact_size(vec2(width, galley.size().y + 28.0), Sense::hover());
    ui.painter().galley(r.min + vec2(20.0, 14.0), galley, c(theme::TEXT_2));
}

/// The pinned digest, or how to get one.
fn digest_card(ui: &mut Ui, core: &Core, state: &Social, width: f32, acts: &mut Vec<Act>) {
    let Some(digest) = &state.digest else {
        let news = news_agent(core);
        let (title, text) = match &news {
            Some(id) => {
                let name = core.world.agent(id).map_or("The news agent".to_string(), |a| a.name.clone());
                (format!("{name} writes your first digest at 07:00"), "It reads your sources and your feed, and the digest waits here every morning.".to_string())
            }
            None => (
                "Get a morning digest".to_string(),
                "A news agent reads the sources you choose and your feed every morning, and pins a short digest here. It learns from what you open and rate.".to_string(),
            ),
        };
        let galley = ui.painter().layout(text, theme::ui(14.0), c(theme::TEXT_2), width - 220.0);
        let h = galley.size().y + 62.0;
        let (r, _) = ui.allocate_exact_size(vec2(width, h), Sense::hover());
        let painter = ui.painter();
        painter.rect(r, CornerRadius::same(14), c([0x16, 0x1A, 0x23]), Stroke::new(1.0, c([0x2A, 0x30, 0x3D])), StrokeKind::Inside);
        painter.text(r.min + vec2(18.0, 16.0), Align2::LEFT_TOP, title, theme::ui_semibold(16.0), c(theme::TEXT));
        painter.galley(r.min + vec2(18.0, 42.0), galley, c(theme::TEXT_2));
        if news.is_none() {
            let b = Rect::from_min_size(pos2(r.max.x - 18.0 - 180.0, r.center().y - 20.0), vec2(180.0, 40.0));
            if state.template_pending {
                disabled(ui, b, "Adding…");
            } else if button(ui, b, "Add the news agent", ButtonStyle::Primary).clicked() {
                acts.push(Act::AddNews);
            }
        }
        return;
    };
    let name = core.world.agent(&digest.agent).map_or_else(|| digest.agent.clone(), |a| a.name.clone());
    let inner = width - 36.0;
    // Measure first, so the card is drawn behind its items.
    let item_h: Vec<f32> = digest
        .items
        .iter()
        .map(|i| {
            let t = ui.painter().layout(i.title.clone(), theme::ui_semibold(15.0), c(theme::TEXT), inner - 40.0).size().y;
            let s = if i.summary.is_empty() {
                0.0
            } else {
                ui.painter().layout(i.summary.clone(), theme::ui(13.5), c(theme::TEXT_2), inner - 40.0).size().y + 4.0
            };
            t + s + 58.0
        })
        .collect();
    let h = 74.0 + item_h.iter().sum::<f32>() + 8.0;
    let (r, _) = ui.allocate_exact_size(vec2(width, h), Sense::hover());
    let painter = ui.painter();
    painter.rect(r, CornerRadius::same(16), c([0x1E, 0x19, 0x10]), Stroke::new(1.0, c([0x5A, 0x44, 0x24])), StrokeKind::Inside);
    painter.text(
        r.min + vec2(18.0, 16.0),
        Align2::LEFT_TOP,
        format!("PINNED · MORNING DIGEST BY {} · {}", name.to_uppercase(), ago(digest.created)),
        theme::mono(10.5),
        c(theme::WORKING),
    );
    painter.text(r.min + vec2(18.0, 36.0), Align2::LEFT_TOP, &digest.title, theme::ui_bold(19.0), c([0xF3, 0xE2, 0xC6]));
    let mut y = r.min.y + 74.0;
    for (item, h) in digest.items.iter().zip(item_h) {
        let row = Rect::from_min_size(pos2(r.min.x + 18.0, y), vec2(inner, h - 8.0));
        let painter = ui.painter();
        painter.rect_filled(row, CornerRadius::same(10), ca([0x0B, 0x0D, 0x12], 120));
        let title = painter.layout(item.title.clone(), theme::ui_semibold(15.0), c(theme::TEXT), inner - 40.0);
        let title_h = title.size().y;
        let title_rect = Rect::from_min_size(row.min + vec2(14.0, 10.0), title.size());
        painter.galley(title_rect.min, title, c(theme::TEXT));
        let mut ty = title_rect.max.y + 4.0;
        if !item.summary.is_empty() {
            let summary = painter.layout(item.summary.clone(), theme::ui(13.5), c(theme::TEXT_2), inner - 40.0);
            ty += summary.size().y;
            painter.galley(pos2(row.min.x + 14.0, title_rect.max.y + 4.0), summary, c(theme::TEXT_2));
        }
        let _ = title_h;
        // Source, opened, and the reactions.
        let meta_y = ty + 8.0;
        let mut source = item.source.clone().unwrap_or_default().to_uppercase();
        if item.opened {
            source = if source.is_empty() { "OPENED".into() } else { format!("{source} · OPENED") };
        }
        painter.text(pos2(row.min.x + 14.0, meta_y + 10.0), Align2::LEFT_CENTER, source, theme::mono(10.0), c([0x8A, 0x91, 0xA0]));
        let less = Rect::from_min_size(pos2(row.max.x - 12.0 - 64.0, meta_y), vec2(64.0, 22.0));
        let more = Rect::from_min_size(pos2(less.min.x - 6.0 - 64.0, meta_y), vec2(64.0, 22.0));
        if chip(ui, more, "More", item.reaction == Some(Reaction::More), theme::WORKING) {
            acts.push(Act::React(digest.id.clone(), item.id.clone(), Reaction::More));
        }
        if chip(ui, less, "Less", item.reaction == Some(Reaction::Less), theme::TEXT_2) {
            acts.push(Act::React(digest.id.clone(), item.id.clone(), Reaction::Less));
        }
        if let Some(url) = &item.url {
            let open = Rect::from_min_size(pos2(more.min.x - 6.0 - 64.0, meta_y), vec2(64.0, 22.0));
            if chip(ui, open, "Open", false, theme::NEEDS_YOU) {
                acts.push(Act::Open(url.clone(), digest.id.clone(), item.id.clone()));
            }
        }
        y += h;
    }
}

/// A small toggle chip. Returns true when clicked.
fn chip(ui: &mut Ui, rect: Rect, text: &str, on: bool, color: [u8; 3]) -> bool {
    let resp = ui.interact(rect, ui.id().with(("chip", text, rect.min.x as i32, rect.min.y as i32)), Sense::click());
    let painter = ui.painter();
    let (fill, stroke, fg) = if on {
        (ca(color, 40), Stroke::new(1.0, ca(color, 160)), c(color))
    } else if resp.hovered() {
        (c([0x1A, 0x1F, 0x2A]), Stroke::new(1.0, c([0x3A, 0x42, 0x52])), c([0xC9, 0xCE, 0xD9]))
    } else {
        (Color32::TRANSPARENT, Stroke::new(1.0, c([0x2A, 0x30, 0x3D])), c([0xB9, 0xBF, 0xCC]))
    };
    painter.rect(rect, CornerRadius::same(7), fill, stroke, StrokeKind::Inside);
    painter.text(rect.center(), Align2::CENTER_CENTER, text, theme::ui(12.5), fg);
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp.clicked()
}

fn post_row(ui: &mut Ui, pictures: &Pictures, post: &Post, me: &str, width: f32) {
    let text_w = width - 92.0;
    let galley = ui.painter().layout(post.text.clone(), theme::ui(14.5), c(theme::TEXT), text_w);
    let h = (galley.size().y + 62.0).max(84.0);
    let (r, _) = ui.allocate_exact_size(vec2(width, h), Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(r, CornerRadius::same(14), c([0x16, 0x1A, 0x23]));
    let pic = Rect::from_min_size(r.min + vec2(14.0, 14.0), vec2(48.0, 48.0));
    picture(painter, pictures, post.author.picture.as_deref(), &post.author.name, pic);
    let x = pic.max.x + 14.0;
    let mine = post.author.id == me;
    let name = if mine { format!("{} (you)", post.author.name) } else { post.author.name.clone() };
    let name_r = painter.text(pos2(x, r.min.y + 14.0), Align2::LEFT_TOP, name, theme::ui_semibold(15.5), c(theme::TEXT));
    let who = match (&post.author.owner, post.author.kind) {
        (Some(owner), _) => owner.clone(),
        (None, ManifestKind::Person) => format!("@{}", post.author.id),
        (None, ManifestKind::Organisation) => post.author.id.clone(),
    };
    let right = r.max.x - 14.0;
    let (vis_text, vis_color) = match post.visibility {
        Visibility::Friends => ("FRIENDS", MINT),
        _ => ("PUBLIC", theme::NEEDS_YOU),
    };
    let vis_w = painter.layout_no_wrap(vis_text.into(), theme::mono(9.5), Color32::WHITE).size().x + 12.0;
    small_pill(painter, pos2(right - vis_w, r.min.y + 22.0), vis_text, vis_color);
    let when = painter.text(
        pos2(right - vis_w - 10.0, r.min.y + 22.0),
        Align2::RIGHT_CENTER,
        ago(post.published),
        theme::mono(10.5),
        c(theme::TEXT_2),
    );
    let who = fit(painter, &who, theme::ui(13.0), when.min.x - name_r.max.x - 22.0);
    painter.text(pos2(name_r.max.x + 10.0, name_r.center().y), Align2::LEFT_CENTER, who, theme::ui(13.0), c(theme::TEXT_2));
    painter.galley(pos2(x, r.min.y + 42.0), galley, c(theme::TEXT));
}

fn side_pane(ui: &mut Ui, core: &Core, state: &mut Social, rect: Rect, acts: &mut Vec<Act>) {
    let painter = ui.painter();
    super::panel(painter, rect, 18);
    painter.text(rect.min + vec2(20.0, 20.0), Align2::LEFT_TOP, "Waiting for you", theme::ui_bold(22.0), c(theme::TEXT));
    let area = Rect::from_min_max(pos2(rect.min.x + 12.0, rect.min.y + 60.0), pos2(rect.max.x - 12.0, rect.max.y - 12.0));
    let mut side = ui.new_child(egui::UiBuilder::new().max_rect(area).layout(egui::Layout::top_down(egui::Align::Min)));
    egui::ScrollArea::vertical().id_salt("feed-side").auto_shrink([false, false]).show(&mut side, |ui| {
        let width = area.width() - 14.0;
        drafts_section(ui, core, state, width, acts);
        requests_section(ui, state, width, acts);
        relations_section(ui, state, width, acts);
        if news_agent(core).is_some() {
            sources_section(ui, state, width, acts);
        }
        ui.add_space(8.0);
        let note = "People and agents you follow show up here in the feed. They do not move into your space as figures.";
        let g = ui.painter().layout(note.into(), theme::ui(12.5), ca(theme::TEXT_2, 200), width - 16.0);
        let (r, _) = ui.allocate_exact_size(vec2(width, g.size().y + 8.0), Sense::hover());
        ui.painter().galley(r.min + vec2(8.0, 4.0), g, ca(theme::TEXT_2, 200));
    });
}

fn section_label(ui: &mut Ui, width: f32, text: &str) {
    let (r, _) = ui.allocate_exact_size(vec2(width, 26.0), Sense::hover());
    label(ui.painter(), r.min + vec2(8.0, 6.0), text);
}

fn drafts_section(ui: &mut Ui, core: &Core, state: &mut Social, width: f32, acts: &mut Vec<Act>) {
    section_label(ui, width, &format!("POST DRAFTS FROM YOUR AGENT · {}", state.drafts.len()));
    if state.drafts.is_empty() {
        let text = if personal_agent(core).is_some() {
            "No drafts. Ask your personal agent to write a post, and it waits here until you approve it."
        } else {
            "Your personal agent will draft posts about you here. Nothing goes out before you approve it."
        };
        quiet_text(ui, width, text);
        return;
    }
    let drafts = state.drafts.clone();
    for d in &drafts {
        let author = core.world.agent(&d.agent).map_or_else(|| d.agent.clone(), |a| a.name.clone());
        let editing = state.edits.contains_key(&d.id);
        let sending = state.sending.contains(&d.id);
        let text = state.edits.get(&d.id).map_or_else(|| d.text.clone(), |(t, _)| t.clone());
        let galley = ui.painter().layout(text.clone(), theme::ui(14.5), c(theme::TEXT), width - 32.0);
        let text_h = if editing { 96.0 } else { galley.size().y };
        let error_h = if d.error.is_some() { 22.0 } else { 0.0 };
        let h = 52.0 + text_h + 56.0 + error_h;
        let (r, _) = ui.allocate_exact_size(vec2(width, h), Sense::hover());
        let painter = ui.painter();
        painter.rect(r, CornerRadius::same(14), c([0x16, 0x1A, 0x23]), Stroke::new(1.0, ca(theme::NEEDS_YOU, 110)), StrokeKind::Inside);
        painter.text(r.min + vec2(16.0, 14.0), Align2::LEFT_TOP, format!("{author} drafted"), theme::ui_semibold(14.5), c(theme::TEXT));
        painter.text(pos2(r.max.x - 16.0, r.min.y + 16.0), Align2::RIGHT_TOP, ago(d.created), theme::mono(10.5), c(theme::TEXT_2));
        let text_rect = Rect::from_min_size(r.min + vec2(16.0, 42.0), vec2(width - 32.0, text_h));
        let visibility = state.edits.get(&d.id).map_or(d.visibility, |(_, v)| *v);
        if editing {
            ui.painter().rect(
                text_rect.expand(4.0),
                CornerRadius::same(10),
                c([0x11, 0x14, 0x1B]),
                Stroke::new(1.0, c([0x2A, 0x30, 0x3D])),
                StrokeKind::Inside,
            );
            if let Some((t, _)) = state.edits.get_mut(&d.id) {
                ui.put(
                    text_rect,
                    egui::TextEdit::multiline(t)
                        .id(Id::new(("draft-edit", &d.id)))
                        .frame(egui::Frame::NONE)
                        .font(theme::ui(14.5))
                        .desired_width(text_rect.width()),
                );
            }
        } else {
            ui.painter().galley(text_rect.min, galley, c(theme::TEXT));
        }
        let y = text_rect.max.y + 14.0;
        // Who may read it.
        let seg = Rect::from_min_size(pos2(r.min.x + 16.0, y), vec2(168.0, 32.0));
        if let Some(v) = visibility_switch(ui, seg, visibility, ("draft-vis", d.id.as_str()), false) {
            let entry = state.edits.entry(d.id.clone()).or_insert_with(|| (d.text.clone(), d.visibility));
            entry.1 = v;
        }
        let approve = Rect::from_min_size(pos2(r.max.x - 16.0 - 104.0, y), vec2(104.0, 32.0));
        let edit = Rect::from_min_size(pos2(approve.min.x - 8.0 - 76.0, y), vec2(76.0, 32.0));
        let discard = Rect::from_min_size(pos2(edit.min.x - 8.0 - 84.0, y), vec2(84.0, 32.0));
        if sending {
            disabled(ui, approve, "Sending…");
        } else if small_button(ui, approve, "Approve", ButtonStyle::Primary) {
            acts.push(Act::Approve(d.id.clone()));
        }
        if !editing && small_button(ui, edit, "Edit", ButtonStyle::Quiet) {
            acts.push(Act::Edit(d.id.clone()));
        }
        if small_button(ui, discard, "Discard", ButtonStyle::Ghost) {
            acts.push(Act::Discard(d.id.clone()));
        }
        if let Some(e) = &d.error {
            let e = fit(ui.painter(), e, theme::mono(11.0), width - 32.0);
            ui.painter().text(pos2(r.min.x + 16.0, y + 42.0), Align2::LEFT_TOP, e, theme::mono(11.0), c(RED));
        }
        ui.add_space(8.0);
    }
}

fn requests_section(ui: &mut Ui, state: &Social, width: f32, acts: &mut Vec<Act>) {
    let requests = state.relations.as_ref().map(|r| r.requests.clone()).unwrap_or_default();
    ui.add_space(6.0);
    section_label(ui, width, &format!("FRIEND REQUESTS · {}", requests.len()));
    if requests.is_empty() {
        let text = if state.handle.is_some() { "Nobody asked you yet." } else { "Friend requests arrive here once you have an account." };
        quiet_text(ui, width, text);
    }
    for r in &requests {
        let (row, _) = ui.allocate_exact_size(vec2(width, 68.0), Sense::hover());
        let painter = ui.painter();
        painter.rect(row, CornerRadius::same(14), c([0x16, 0x1A, 0x23]), Stroke::new(1.0, ca(MINT, 70)), StrokeKind::Inside);
        let pic = Rect::from_min_size(row.min + vec2(12.0, 12.0), vec2(44.0, 44.0));
        picture(painter, &state.pictures, r.actor.picture.as_deref(), &r.actor.name, pic);
        painter.text(pos2(pic.max.x + 12.0, row.min.y + 14.0), Align2::LEFT_TOP, &r.actor.name, theme::ui_semibold(15.0), c(theme::TEXT));
        painter.text(
            pos2(pic.max.x + 12.0, row.min.y + 38.0),
            Align2::LEFT_TOP,
            format!("@{} · asked {}", r.actor.id, ago(r.since).to_lowercase()),
            theme::ui(12.5),
            c(theme::TEXT_2),
        );
        let accept = Rect::from_min_size(pos2(row.max.x - 12.0 - 92.0, row.center().y - 16.0), vec2(92.0, 32.0));
        let decline = Rect::from_min_size(pos2(accept.min.x - 8.0 - 92.0, accept.min.y), vec2(92.0, 32.0));
        if small_button(ui, accept, "Accept", ButtonStyle::Primary) {
            acts.push(Act::Accept(r.actor.id.clone()));
        }
        if small_button(ui, decline, "Decline", ButtonStyle::Quiet) {
            acts.push(Act::Decline(r.actor.id.clone()));
        }
        ui.add_space(6.0);
    }
}

fn relations_section(ui: &mut Ui, state: &Social, width: f32, acts: &mut Vec<Act>) {
    let Some(rel) = &state.relations else { return };
    ui.add_space(6.0);
    let friends: Vec<&Relation> = rel.follows.iter().filter(|r| r.friend).collect();
    let following: Vec<&Relation> = rel.follows.iter().filter(|r| !r.friend).collect();
    section_label(
        ui,
        width,
        &format!("FRIENDS · {} · FOLLOWING · {} · FOLLOWERS · {}", friends.len(), following.len(), rel.followers.len()),
    );
    let rows: Vec<(&Relation, &str)> = friends.iter().map(|r| (*r, "FRIEND")).chain(following.iter().map(|r| (*r, "FOLLOWING"))).collect();
    if rows.is_empty() {
        quiet_text(ui, width, "You follow nobody yet.");
    }
    for (r, kind) in rows {
        let (row, _) = ui.allocate_exact_size(vec2(width, 44.0), Sense::hover());
        let painter = ui.painter();
        let pic = Rect::from_min_size(row.min + vec2(8.0, 6.0), vec2(32.0, 32.0));
        picture(painter, &state.pictures, r.actor.picture.as_deref(), &r.actor.name, pic);
        let name =
            painter.text(pos2(pic.max.x + 10.0, row.center().y), Align2::LEFT_CENTER, &r.actor.name, theme::ui(14.0), c(theme::TEXT));
        let sub = r.actor.owner.clone().unwrap_or_else(|| format!("@{}", r.actor.id));
        let unfollow = Rect::from_min_size(pos2(row.max.x - 8.0 - 84.0, row.center().y - 14.0), vec2(84.0, 28.0));
        let pill_color = if kind == "FRIEND" { MINT } else { theme::NEEDS_YOU };
        let pill_w = painter.layout_no_wrap(kind.into(), theme::mono(9.5), Color32::WHITE).size().x + 12.0;
        let pill_x = unfollow.min.x - 10.0 - pill_w;
        small_pill(painter, pos2(pill_x, row.center().y), kind, pill_color);
        let sub = fit(painter, &sub, theme::ui(12.5), pill_x - name.max.x - 20.0);
        painter.text(pos2(name.max.x + 8.0, row.center().y), Align2::LEFT_CENTER, sub, theme::ui(12.5), c(theme::TEXT_2));
        if small_button(ui, unfollow, if kind == "FRIEND" { "Unfriend" } else { "Unfollow" }, ButtonStyle::Ghost) {
            acts.push(Act::Unfollow(r.actor.id.clone()));
        }
    }
    if !rel.pending.is_empty() {
        let names: Vec<String> = rel.pending.iter().map(|r| r.actor.name.clone()).collect();
        quiet_text(ui, width, &format!("Waiting for an answer from {}.", names.join(", ")));
    }
}

fn sources_section(ui: &mut Ui, state: &mut Social, width: f32, acts: &mut Vec<Act>) {
    ui.add_space(6.0);
    section_label(ui, width, "NEWS SOURCES · ONE PER LINE");
    let Some(text) = state.sources.as_mut() else { return };
    let (r, _) = ui.allocate_exact_size(vec2(width, 120.0), Sense::hover());
    ui.painter().rect(r, CornerRadius::same(12), c([0x11, 0x14, 0x1B]), Stroke::new(1.0, c([0x2A, 0x30, 0x3D])), StrokeKind::Inside);
    let before = text.clone();
    ui.put(
        r.shrink2(vec2(12.0, 10.0)),
        egui::TextEdit::multiline(text)
            .id(Id::new("news-sources"))
            .frame(egui::Frame::NONE)
            .font(theme::mono(12.5))
            .desired_width(r.width() - 24.0),
    );
    if *text != before {
        state.sources_saved = false;
    }
    let (row, _) = ui.allocate_exact_size(vec2(width, 44.0), Sense::hover());
    let save = Rect::from_min_size(pos2(row.max.x - 104.0, row.min.y + 8.0), vec2(104.0, 32.0));
    let hint = "Web addresses, feeds or topics. The news agent reads them every morning.";
    ui.painter().text(pos2(row.min.x + 8.0, save.center().y), Align2::LEFT_CENTER, hint, theme::ui(12.5), c(theme::TEXT_2));
    if small_button(ui, save, if state.sources_saved { "Saved" } else { "Save" }, ButtonStyle::Quiet) {
        acts.push(Act::SaveSources);
    }
}

fn quiet_text(ui: &mut Ui, width: f32, text: &str) {
    let g = ui.painter().layout(text.into(), theme::ui(13.5), c(theme::TEXT_2), width - 16.0);
    let (r, _) = ui.allocate_exact_size(vec2(width, g.size().y + 12.0), Sense::hover());
    ui.painter().galley(r.min + vec2(8.0, 4.0), g, c(theme::TEXT_2));
}

/// A button with a smaller font than [`button`], for cards.
fn small_button(ui: &mut Ui, rect: Rect, text: &str, style: ButtonStyle) -> bool {
    let resp = ui.interact(rect, ui.id().with(("small", text, rect.min.x as i32, rect.min.y as i32)), Sense::click());
    let hovered = resp.hovered();
    let (fill, stroke, fg) = match style {
        ButtonStyle::Primary => (if hovered { c([0xA6, 0xDD, 0xFF]) } else { c(theme::NEEDS_YOU) }, Stroke::NONE, c([0x0B, 0x1A, 0x26])),
        ButtonStyle::Selected => (ca(MINT, 36), Stroke::new(1.0, ca(MINT, 140)), c(MINT)),
        ButtonStyle::Ghost => (if hovered { c([0x1A, 0x1F, 0x2A]) } else { Color32::TRANSPARENT }, Stroke::NONE, c(theme::TEXT_2)),
        ButtonStyle::Quiet => (
            if hovered { c([0x1A, 0x1F, 0x2A]) } else { Color32::TRANSPARENT },
            Stroke::new(1.0, if hovered { c([0x3A, 0x42, 0x52]) } else { c([0x2A, 0x30, 0x3D]) }),
            c([0xC9, 0xCE, 0xD9]),
        ),
    };
    let painter = ui.painter();
    painter.rect(rect, CornerRadius::same(9), fill, stroke, StrokeKind::Inside);
    let font = if style == ButtonStyle::Primary { theme::ui_bold(13.5) } else { theme::ui(13.5) };
    painter.text(rect.center(), Align2::CENTER_CENTER, text, font, fg);
    if hovered {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp.clicked()
}

fn disabled(ui: &Ui, rect: Rect, text: &str) {
    let painter = ui.painter();
    painter.rect(rect, CornerRadius::same(9), c([0x1A, 0x1E, 0x28]), Stroke::NONE, StrokeKind::Inside);
    painter.text(rect.center(), Align2::CENTER_CENTER, text, theme::ui_bold(13.5), c([0x5B, 0x61, 0x70]));
}

/// Public, friends and private as a segmented switch. `private` offers the
/// third choice, posts have only two. Returns the new value when clicked.
pub fn visibility_switch(ui: &mut Ui, rect: Rect, value: Visibility, id: impl egui::AsId, private: bool) -> Option<Visibility> {
    let options: &[(Visibility, &str, [u8; 3])] = if private {
        &[
            (Visibility::Public, "Public", theme::NEEDS_YOU),
            (Visibility::Friends, "Friends", MINT),
            (Visibility::Private, "Private", theme::TEXT_2),
        ]
    } else {
        &[(Visibility::Public, "Public", theme::NEEDS_YOU), (Visibility::Friends, "Friends", MINT)]
    };
    ui.painter().rect(rect, CornerRadius::same(10), c([0x0E, 0x11, 0x17]), Stroke::new(1.0, c(theme::BORDER)), StrokeKind::Inside);
    let w = (rect.width() - 6.0) / options.len() as f32;
    let mut picked = None;
    let id = Id::new(id);
    for (i, (v, text, color)) in options.iter().enumerate() {
        let seg = Rect::from_min_size(pos2(rect.min.x + 3.0 + i as f32 * w, rect.min.y + 3.0), vec2(w, rect.height() - 6.0));
        let resp = ui.interact(seg, id.with(i), Sense::click());
        let on = value == *v;
        let painter = ui.painter();
        if on {
            painter.rect(seg, CornerRadius::same(8), ca(*color, 34), Stroke::new(1.0, ca(*color, 120)), StrokeKind::Inside);
        } else if resp.hovered() {
            painter.rect_filled(seg, CornerRadius::same(8), c([0x1A, 0x1F, 0x2A]));
        }
        painter.text(seg.center(), Align2::CENTER_CENTER, *text, theme::ui(12.5), if on { c(*color) } else { c([0x8A, 0x91, 0xA0]) });
        if resp.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        if resp.clicked() && !on {
            picked = Some(*v);
        }
    }
    picked
}

/// "5 MIN AGO", "3 H AGO", "2 D AGO" or "3 WEEKS AGO".
pub fn ago(ms: u64) -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
    let s = now.saturating_sub(ms) / 1000;
    match s {
        0..60 => "JUST NOW".into(),
        60..3600 => format!("{} MIN AGO", s / 60),
        3600..86_400 => format!("{} H AGO", s / 3600),
        86_400..1_209_600 => format!("{} D AGO", s / 86_400),
        _ => format!("{} WEEKS AGO", s / 604_800),
    }
}

/// Waits until every picture loaded, for snapshots.
pub fn wait_for_pictures(core: &mut Core, timeout: Duration) {
    core.ui.social.pictures.wait(timeout);
}

/// Follow, Befriend and the friendship state of a search result, in the
/// search header left of `right`. Returns the left edge of what it drew.
pub fn index_buttons(ui: &mut Ui, core: &mut Core, entry: &IndexEntry, right: f32) -> f32 {
    let m = &entry.manifest;
    let person = m.kind == ManifestKind::Person;
    let actor = if person { m.owner.domain.clone() } else { m.id.clone() };
    let s = &core.ui.social;
    if s.handle.as_deref() == Some(actor.as_str()) {
        let w = ui.painter().layout_no_wrap("THIS IS YOU".into(), theme::mono(9.5), Color32::WHITE).size().x + 12.0;
        small_pill(ui.painter(), pos2(right - w, 36.0), "THIS IS YOU", MINT);
        return right - w;
    }
    let rel = s.relations.clone().unwrap_or_default();
    let signed_in = s.handle.is_some();
    let busy = s.social_request.is_some();
    let mut x = right;
    let mut action = None;
    if person {
        let asked_me = rel.requests.iter().any(|r| r.actor.id == actor);
        let (text, style, act) = if rel.is_friend(&actor) {
            ("Friends", ButtonStyle::Selected, None)
        } else if asked_me {
            ("Accept", ButtonStyle::Primary, Some(SocialAction::Accept { handle: actor.clone() }))
        } else if rel.asked(&actor) {
            ("Asked", ButtonStyle::Selected, None)
        } else {
            ("Befriend", ButtonStyle::Quiet, Some(SocialAction::Befriend { handle: actor.clone() }))
        };
        let r = Rect::from_min_size(pos2(x - 116.0, 14.0), vec2(116.0, 44.0));
        x = r.min.x - 10.0;
        if !signed_in {
            disabled_big(ui, r, text);
        } else if button(ui, r, text, style).clicked() && !busy {
            action = act;
        }
    }
    let following = rel.follows(&actor);
    let r = Rect::from_min_size(pos2(x - 116.0, 14.0), vec2(116.0, 44.0));
    let (text, style, act) = if following {
        ("Following", ButtonStyle::Selected, SocialAction::Unfollow { actor: actor.clone() })
    } else {
        ("Follow", ButtonStyle::Quiet, SocialAction::Follow { actor: actor.clone() })
    };
    if !signed_in {
        disabled_big(ui, r, text);
        if ui.interact(r, Id::new("follow-why"), Sense::hover()).hovered() {
            egui::Tooltip::always_open(ui.ctx().clone(), ui.layer_id(), Id::new("follow-why-tip"), egui::PopupAnchor::Pointer).show(|ui| {
                ui.label("Publish your profile first, so the index knows who follows.");
            });
        }
    } else if button(ui, r, text, style).clicked() && !busy {
        action = Some(act);
    }
    if let Some(action) = action {
        social_action(core, action);
    }
    r.min.x
}

fn disabled_big(ui: &Ui, rect: Rect, text: &str) {
    let painter = ui.painter();
    painter.rect(rect, CornerRadius::same(10), c([0x1A, 0x1E, 0x28]), Stroke::NONE, StrokeKind::Inside);
    painter.text(rect.center(), Align2::CENTER_CENTER, text, theme::ui(14.5), c([0x5B, 0x61, 0x70]));
}

/// The profile of a person in the search detail pane: photos, headline and
/// what they share. `rect` is the space below the animations.
pub fn person_details(ui: &mut Ui, core: &Core, pictures: &Pictures, entry: &IndexEntry, profile: &Profile, rect: Rect) {
    let painter = ui.painter();
    let x = rect.min.x;
    let mut y = rect.min.y;
    label(painter, pos2(x, y), "PROFILE");
    let friend = core.ui.social.relations.as_ref().is_some_and(|r| r.is_friend(&entry.manifest.owner.domain));
    let note = if friend { "WHAT FRIENDS SEE" } else { "WHAT EVERYONE SEES" };
    painter.text(pos2(rect.max.x, y), Align2::RIGHT_TOP, note, theme::mono(11.0), c(if friend { MINT } else { theme::TEXT_2 }));
    y += 22.0;
    // Photos.
    let size = 92.0;
    let mut px = x;
    for photo in profile.photos.iter().take(6) {
        let r = Rect::from_min_size(pos2(px, y), vec2(size, size));
        picture(painter, pictures, Some(&photo.url), &entry.manifest.name, r);
        if photo.visibility == Visibility::Friends {
            small_pill(painter, pos2(r.min.x + 6.0, r.max.y - 12.0), "FRIENDS", MINT);
        }
        px += size + 10.0;
    }
    let text_x = if profile.photos.is_empty() { x } else { px + 6.0 };
    let width = rect.max.x - text_x;
    let mut ty = y;
    if let Some(h) = &profile.headline {
        let g = painter.layout(h.value.clone(), theme::ui_semibold(16.0), c(theme::TEXT), width);
        let gh = g.size().y;
        painter.galley(pos2(text_x, ty), g, c(theme::TEXT));
        ty += gh + 8.0;
    }
    let facts = facts(profile);
    for (what, value, vis) in facts {
        if ty > rect.max.y - 18.0 {
            break;
        }
        let w = painter.text(pos2(text_x, ty), Align2::LEFT_TOP, what, theme::mono(10.5), c(theme::TEXT_2)).width();
        let value = fit(painter, &value, theme::ui(13.5), width - w - 90.0);
        let v = painter.text(pos2(text_x + 86.0, ty - 2.0), Align2::LEFT_TOP, value, theme::ui(13.5), c([0xC9, 0xCE, 0xD9]));
        if vis == Visibility::Friends {
            small_pill(painter, pos2(v.max.x + 8.0, v.center().y), "FRIENDS", MINT);
        }
        ty += 22.0;
    }
    if profile.photos.is_empty() && profile.headline.is_none() && ty == y {
        painter.text(pos2(x, y), Align2::LEFT_TOP, "This person shares nothing publicly yet.", theme::ui(14.0), c(theme::TEXT_2));
    }
}

/// The profile as (label, text, visibility) lines.
pub fn facts(p: &Profile) -> Vec<(&'static str, String, Visibility)> {
    let years = |from: Option<i32>, to: Option<i32>| match (from, to) {
        (Some(f), Some(t)) => format!(" · {f} to {t}"),
        (Some(f), None) => format!(" · since {f}"),
        (None, Some(t)) => format!(" · until {t}"),
        (None, None) => String::new(),
    };
    let mut out = Vec::new();
    for w in &p.work {
        let role = w.role.as_ref().map(|r| format!("{r}, ")).unwrap_or_default();
        out.push(("WORK", format!("{role}{}{}", w.org, years(w.from, w.to)), w.visibility));
    }
    for e in &p.education {
        let degree = e.degree.as_ref().map(|d| format!("{d}, ")).unwrap_or_default();
        out.push(("SCHOOL", format!("{degree}{}{}", e.school, years(e.from, e.to)), e.visibility));
    }
    for pl in &p.places {
        out.push(("PLACE", pl.value.clone(), pl.visibility));
    }
    let public: Vec<String> = p.interests.iter().filter(|i| i.visibility == Visibility::Public).map(|i| i.value.clone()).collect();
    if !public.is_empty() {
        out.push(("INTERESTS", public.join(", "), Visibility::Public));
    }
    let friends: Vec<String> = p.interests.iter().filter(|i| i.visibility == Visibility::Friends).map(|i| i.value.clone()).collect();
    if !friends.is_empty() {
        out.push(("INTERESTS", friends.join(", "), Visibility::Friends));
    }
    for l in &p.links {
        out.push(("LINK", l.url.clone(), l.visibility));
    }
    out
}
