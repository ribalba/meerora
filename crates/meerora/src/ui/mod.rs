//! The 2D parts drawn with egui on top of the scene: the space view
//! (screen 1) and the agent view (screens 2 and 2b).

pub mod agent;
pub mod approvals;
pub mod apps;
pub mod browser;
pub mod builder;
pub mod commands;
pub mod config_card;
pub mod device_list;
pub mod devices;
pub mod editor;
pub mod feed;
pub mod graveyard;
pub mod index;
pub mod profile;
pub mod rooms;
pub mod space;
pub mod switcher;
pub mod tags;
pub mod terminal;
pub mod tiles;
pub mod voice;
pub mod window;

use std::collections::HashSet;
use std::f32::consts::{PI, TAU};

use egui::{Align2, Color32, CornerRadius, FontId, Painter, Pos2, Rect, Response, Sense, Shape, Stroke, StrokeKind, Ui, pos2, vec2};
use meerora_protocol::{Command, RingState};

use crate::engine::Core;
use crate::theme::{self, c, ca};
use crate::world::Agent;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum View {
    Space,
    Agent(String),
    /// Creating a new agent (screen 3).
    Builder,
    /// Searching the meerora index for an external agent (D25).
    Index,
    /// Posts, the news digest, drafts and friend requests (D26 to D28).
    Feed,
    /// The personal profile: the interview and the review (D26).
    Profile,
    /// The retired agents, with Bring back and Delete for good (graveyard.rs).
    Graveyard,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layout {
    Tiled,
    Fullscreen,
}

/// One app instance of the agent view: a tab, a tile and its state.
/// Terminal, Browser, Editor, Files, Viewer and Git run as several
/// instances, numbered from 1. Chat, Changes and Activity run once (see [`multi`]).
///
/// Every instance has a handle string (D30, docs/design/agent-apps.md):
/// `chat`, `terminal:2`, `window:7` and so on, see [`AppTab::handle`] and
/// [`AppTab::parse_handle`]. In `layouts.json` instance 1 is the plain name
/// (`"terminal"`), so layouts from before instances still load, and the
/// others are `{"terminal":2}` like a window's `{"window":7}`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AppTab {
    Chat,
    Editor(u32),
    Terminal(u32),
    Browser(u32),
    Files(u32),
    Viewer(u32),
    Changes,
    /// A repository to review, edit, stage, commit and push (ui/apps/git.rs).
    Git(u32),
    Activity,
    /// A real app's window on the agent's desktop, by compositor window id
    /// (D15 phase 2, ui/window.rs). Its title comes from the window, see [`tab_title`].
    Window(u32),
}

/// Whether `app` can run as several instances in one agent view.
pub fn multi(app: meerora_protocol::App) -> bool {
    use meerora_protocol::App;
    matches!(app, App::Editor | App::Terminal | App::Browser | App::Files | App::Viewer | App::Git)
}

/// The name of `app` in handles and in `layouts.json`.
fn app_key(app: meerora_protocol::App) -> &'static str {
    use meerora_protocol::App;
    match app {
        App::Chat => "chat",
        App::Editor => "editor",
        App::Terminal => "terminal",
        App::Browser => "browser",
        App::Files => "files",
        App::Viewer => "viewer",
        App::Changes => "changes",
        App::Git => "git",
        App::Activity => "activity",
    }
}

/// A string for egui ids and state that belongs to instance `n` of an app of
/// `agent`. Instance 1 keeps the plain agent id, as before instances.
pub fn instance_key(agent: &str, n: u32) -> String {
    if n <= 1 { agent.to_string() } else { format!("{agent}#{n}") }
}

impl AppTab {
    /// Instance 1 of `app`.
    pub fn from_app(app: meerora_protocol::App) -> Self {
        Self::of(app, 1)
    }

    /// Instance `n` of `app`. Apps that run once ignore `n`.
    pub fn of(app: meerora_protocol::App, n: u32) -> Self {
        use meerora_protocol::App;
        match app {
            App::Chat => AppTab::Chat,
            App::Editor => AppTab::Editor(n),
            App::Terminal => AppTab::Terminal(n),
            App::Browser => AppTab::Browser(n),
            App::Files => AppTab::Files(n),
            App::Viewer => AppTab::Viewer(n),
            App::Changes => AppTab::Changes,
            App::Git => AppTab::Git(n),
            App::Activity => AppTab::Activity,
        }
    }

    /// The built-in app this is an instance of. None for a window.
    pub fn app(self) -> Option<meerora_protocol::App> {
        use meerora_protocol::App;
        Some(match self {
            AppTab::Chat => App::Chat,
            AppTab::Editor(_) => App::Editor,
            AppTab::Terminal(_) => App::Terminal,
            AppTab::Browser(_) => App::Browser,
            AppTab::Files(_) => App::Files,
            AppTab::Viewer(_) => App::Viewer,
            AppTab::Changes => App::Changes,
            AppTab::Git(_) => App::Git,
            AppTab::Activity => App::Activity,
            AppTab::Window(_) => return None,
        })
    }

    /// The instance number of a built-in app, 1 for apps that run once and
    /// for windows, whose id is not an instance number.
    pub fn instance(self) -> u32 {
        match self {
            AppTab::Editor(n) | AppTab::Terminal(n) | AppTab::Browser(n) | AppTab::Files(n) | AppTab::Viewer(n) | AppTab::Git(n) => n,
            AppTab::Chat | AppTab::Changes | AppTab::Activity | AppTab::Window(_) => 1,
        }
    }

    /// The handle of this instance (D30): `chat`, `changes`, `activity`,
    /// `terminal:1`, `browser:2`, `editor:1`, `files:1`, `viewer:3` or `window:7`.
    pub fn handle(self) -> String {
        match (self, self.app()) {
            (AppTab::Window(id), _) => format!("window:{id}"),
            (_, Some(app)) if multi(app) => format!("{}:{}", app_key(app), self.instance()),
            (_, Some(app)) => app_key(app).to_string(),
            (_, None) => unreachable!("only windows have no app"),
        }
    }

    /// Reads a handle of [`AppTab::handle`]. A multi-instance app without a
    /// number is instance 1, so `terminal` is `terminal:1`. Instance 0, a
    /// number on an app that runs once and unknown names are refused.
    pub fn parse_handle(text: &str) -> Option<Self> {
        let (name, number) = match text.trim().split_once(':') {
            Some((name, number)) => (name, Some(number.parse::<u32>().ok()?)),
            None => (text.trim(), None),
        };
        if name == "window" {
            return number.map(AppTab::Window);
        }
        let app = meerora_protocol::App::ALL.into_iter().find(|a| app_key(*a) == name)?;
        match number {
            Some(n) if multi(app) && n >= 1 => Some(Self::of(app, n)),
            Some(_) => None,
            None => Some(Self::of(app, 1)),
        }
    }

    /// The name of the app on tabs, tile heads and in the launcher, without
    /// the instance number (see [`AppTab::label`]).
    pub fn title(self) -> &'static str {
        match self {
            AppTab::Chat => "Chat",
            AppTab::Editor(_) => "Editor",
            AppTab::Terminal(_) => "Terminal",
            AppTab::Browser(_) => "Browser",
            AppTab::Files(_) => "Files",
            AppTab::Viewer(_) => "Viewer",
            AppTab::Changes => "Changes",
            AppTab::Git(_) => "Git",
            AppTab::Activity => "Activity",
            AppTab::Window(_) => "Window",
        }
    }

    /// The title with the instance number from 2 on: "Terminal",
    /// "Terminal 2". Windows show their own title, see [`tab_title`].
    pub fn label(self) -> String {
        match self.instance() {
            n if n >= 2 => format!("{} {n}", self.title()),
            _ => self.title().to_string(),
        }
    }

    /// One line for the launcher.
    pub fn blurb(self) -> &'static str {
        match self {
            AppTab::Chat => "Talk to the agent",
            AppTab::Editor(_) => "Edit files and review the agent's changes",
            AppTab::Terminal(_) => "A shell in the agent's directory",
            AppTab::Browser(_) => "Web pages, following what the agent opens",
            AppTab::Files(_) => "Open, find and hand over files",
            AppTab::Viewer(_) => "Images, Markdown and other results",
            AppTab::Changes => "What each run changed, as git history",
            AppTab::Git(_) => "Review, edit, stage, commit and push a repository",
            AppTab::Activity => "Runs, schedule, budget and every action",
            AppTab::Window(_) => "An app on the agent's desktop",
        }
    }
}

impl std::fmt::Display for AppTab {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.handle())
    }
}

impl std::str::FromStr for AppTab {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse_handle(s).ok_or_else(|| format!("{s:?} is not an app handle such as chat, terminal:2 or window:7"))
    }
}

impl serde::Serialize for AppTab {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let (name, number) = match (self, self.app()) {
            (AppTab::Window(id), _) => ("window", Some(*id)),
            (_, Some(app)) => (app_key(app), (self.instance() > 1).then_some(self.instance())),
            (_, None) => unreachable!("only windows have no app"),
        };
        match number {
            Some(n) => {
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry(name, &n)?;
                map.end()
            }
            None => serializer.serialize_str(name),
        }
    }
}

impl<'de> serde::Deserialize<'de> for AppTab {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(serde::Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Name(String),
            Numbered(std::collections::BTreeMap<String, u32>),
        }
        let handle = match Repr::deserialize(deserializer)? {
            Repr::Name(name) => name,
            Repr::Numbered(map) if map.len() == 1 => map.into_iter().map(|(name, n)| format!("{name}:{n}")).collect(),
            Repr::Numbered(_) => return Err(serde::de::Error::custom("an app is one name with its number")),
        };
        AppTab::parse_handle(&handle).ok_or_else(|| serde::de::Error::custom(format!("unknown app {handle:?}")))
    }
}

/// The name on a tab and its tile head. Windows show their own title.
pub fn tab_title(core: &Core, tab: AppTab) -> String {
    match tab {
        AppTab::Window(id) => window::title(core, id),
        _ => tab.label(),
    }
}

pub struct Toast {
    pub text: String,
    pub until: f64,
    pub error: bool,
}

pub struct UiState {
    pub view: View,
    pub layout: Layout,
    pub tab: AppTab,
    pub show_stats: bool,
    pub toasts: Vec<Toast>,
    pub daemon_connected: bool,
    /// Agents whose "needs you" card was put off with Later.
    pub dismissed: HashSet<String>,
    /// Open files, review state and control of every agent's editor tile.
    pub editors: editor::Editors,
    /// Unsent chat text per agent.
    pub drafts: std::collections::HashMap<String, String>,
    pub builder: Option<builder::BuilderState>,
    pub index: Option<index::IndexState>,
    /// Capability requests and grants (D8).
    pub approvals: approvals::Approvals,
    /// The account, the profile, the feed and the digest (D26 to D28).
    pub social: feed::Social,
    /// Text of the "talk to any agent" field.
    pub talk: String,
    /// How far the space view was dragged, in world units.
    pub view_pan: meerora_geom::Vec3,
    /// Zoom of the space view, 1 is the space script's framing.
    pub view_zoom: f32,
    /// Degrees the space view is turned around the vertical axis and tilted,
    /// on top of the space script's camera.
    pub view_yaw: f32,
    pub view_pitch: f32,
    pub fps: f32,
    /// Tile layouts per agent, read from `layouts.json` on first use (D11).
    pub tiles: Option<std::collections::HashMap<String, tiles::TileLayout>>,
    /// The tile dragged by its head to move it.
    pub tile_drag: Option<AppTab>,
    /// The app launcher of the agent view is open.
    pub launcher: bool,
    /// An app to open in the agent view with the next frame (`agent::open_later`).
    pub open_later: Option<AppTab>,
    /// Agents by when their view was last open, most recent first (Alt+Tab).
    pub recent_agents: Vec<String>,
    /// The agent switcher while Alt is held after Alt+Tab (ui/switcher.rs).
    pub switcher: Option<switcher::Switcher>,
    /// Whether the sidebar's devices section is folded, read on first use (ui/device_list.rs).
    pub devices_collapsed: std::sync::OnceLock<bool>,
    /// The instance of each multi-instance app an agent's view selected last,
    /// where other apps open things (`agent::target`).
    pub last_instance: std::collections::HashMap<(String, meerora_protocol::App), u32>,
    /// Folder, selection and pending work of every agent's Files app.
    pub files: apps::files::Files,
    /// The shown file of every agent's Viewer app.
    pub viewer: apps::viewer::Viewers,
    /// History, selection and pending undo of every agent's Changes app.
    pub changes: apps::changes::Changes,
    /// Files read and the action filter of every agent's Activity app.
    pub activity: apps::activity::Activity,
    /// Repositories, status, diffs and commit messages of every Git app.
    pub git: apps::git::Gits,
    /// The graveyard's questions and the commands that wait for the daemon.
    pub graveyard: graveyard::GraveyardUi,
    /// Push to talk, the mic buttons and replies read aloud (D16, ui/voice.rs).
    pub voice: voice::VoiceUi,
    /// The command box on Alt+Space (ui/commands).
    pub commands: commands::CommandBox,
    /// The helper's configuration changes that wait for Apply or Discard (ui/config_card.rs).
    pub config_changes: config_card::ConfigCards,
}

impl UiState {
    pub fn new() -> Self {
        Self {
            view: View::Space,
            layout: Layout::Tiled,
            tab: AppTab::Terminal(1),
            show_stats: false,
            toasts: Vec::new(),
            daemon_connected: false,
            dismissed: HashSet::new(),
            editors: editor::Editors::new(),
            drafts: Default::default(),
            builder: None,
            index: None,
            approvals: approvals::Approvals::default(),
            social: feed::Social::default(),
            talk: String::new(),
            view_pan: meerora_geom::Vec3::ZERO,
            view_zoom: 1.0,
            view_yaw: 0.0,
            view_pitch: 0.0,
            fps: 0.0,
            tiles: None,
            tile_drag: None,
            launcher: false,
            open_later: None,
            recent_agents: Vec::new(),
            switcher: None,
            devices_collapsed: std::sync::OnceLock::new(),
            last_instance: Default::default(),
            files: Default::default(),
            viewer: Default::default(),
            changes: Default::default(),
            activity: Default::default(),
            git: Default::default(),
            graveyard: Default::default(),
            voice: Default::default(),
            commands: Default::default(),
            config_changes: Default::default(),
        }
    }

    pub fn toast(&mut self, now: f64, text: impl Into<String>, error: bool) {
        let text = text.into();
        self.toasts.retain(|t| t.text != text);
        self.toasts.push(Toast { text, until: now + if error { 12.0 } else { 5.0 }, error });
    }
}

/// Builds the whole UI for one frame.
pub fn draw(ui: &mut Ui, core: &mut Core) {
    let now = core.world.time;
    core.ui.toasts.retain(|t| t.until > now);
    window::begin(ui, core);
    // Before everything else, so the open box takes its keys first and draws on top.
    commands::draw(ui, core);
    handle_global_keys(ui, core);
    voice::frame(ui, core);
    // The most recently viewed agent is the default of "Ask an agent to build a device".
    device_list::note_view(ui.ctx(), &core.ui.view);
    // Before the view, so Esc closes only the question.
    graveyard::draw_confirm(ui, core);
    match core.ui.view.clone() {
        View::Space => space::draw(ui, core),
        View::Agent(id) => agent::draw(ui, core, &id),
        View::Builder => builder::draw(ui, core),
        View::Index => index::draw(ui, core),
        View::Feed => feed::draw(ui, core),
        View::Profile => profile::draw(ui, core),
        View::Graveyard => graveyard::draw(ui, core),
    }
    // Over every view, the helper's configuration changes (ui/config_card.rs).
    config_card::draw(ui, core);
    window::end(ui, core);
    voice::draw_overlay(ui, core);
    draw_toasts(ui, core);
    if core.ui.show_stats {
        draw_stats(ui, core);
    }
}

fn handle_global_keys(ui: &mut Ui, core: &mut Core) {
    let (f3, f2) = ui.input(|i| (i.key_pressed(egui::Key::F3), i.key_pressed(egui::Key::F2)));
    if f3 {
        core.ui.show_stats = !core.ui.show_stats;
    }
    // F2 renames in the Files app while it has the keyboard.
    let renaming = matches!(&core.ui.view, View::Agent(id) if apps::files::takes_f2(core, ui.ctx(), id));
    if f2 && matches!(core.ui.view, View::Agent(_)) && !renaming {
        core.ui.layout = if core.ui.layout == Layout::Tiled { Layout::Fullscreen } else { Layout::Tiled };
    }
    // Alt+Tab reaches meerora while desktop shortcuts are inhibited (D15
    // phase 1). It switches between agents (ui/switcher.rs).
    switcher::handle(ui, core);
    // Alt+` cycles the tabs of the agent view, Alt+Shift+` goes back, like
    // the windows of one app on a desktop.
    let (back, forward) = ui.input_mut(|i| {
        (
            i.consume_key(egui::Modifiers::ALT | egui::Modifiers::SHIFT, egui::Key::Backtick),
            i.consume_key(egui::Modifiers::ALT, egui::Key::Backtick),
        )
    });
    if (back || forward)
        && let View::Agent(id) = core.ui.view.clone()
    {
        agent::cycle_tab(core, &id, if back { -1 } else { 1 }, agent::content_rect(ui.ctx().content_rect()));
    }
}

/// Switches to an agent's view and tells the daemon the user attended to it.
pub fn open_agent(core: &mut Core, id: &str) {
    switcher::note_viewed(&mut core.ui.recent_agents, id);
    core.ui.view = View::Agent(id.to_string());
    core.ui.dismissed.remove(id);
    if core.world.agent(id).is_some_and(|a| a.ring == RingState::NeedsYou)
        && let Some(link) = &core.link
    {
        link.send(&Command::Ack { agent: id.to_string() });
    }
}

/// Opens the builder for a new agent (screen 3).
pub fn open_builder(core: &mut Core) {
    let home = core
        .world
        .space
        .as_ref()
        .and_then(|s| s.desc.stations.iter().find(|st| !st.front && st.id != "door").map(|st| st.id.clone()))
        .unwrap_or_else(|| "door".into());
    core.ui.builder = Some(builder::BuilderState::new(core.world.time, home));
    core.ui.view = View::Builder;
}

/// Label and colour of an agent's status pill, as in the sidebar of screen 1.
pub fn status(a: &Agent) -> (String, [u8; 3]) {
    if !a.online {
        return ("OFFLINE".into(), theme::RESTING);
    }
    match a.ring {
        RingState::NeedsYou => ("NEEDS YOU".into(), theme::NEEDS_YOU),
        _ if a.system => ("SYSTEM".into(), theme::SYSTEM_2),
        RingState::Working => match a.progress {
            Some(p) => (format!("{}%", (p * 100.0).round() as i32), theme::WORKING),
            None => ("WORKING".into(), theme::WORKING),
        },
        RingState::Resting => ("RESTING".into(), [0xB9, 0xBF, 0xCC]),
    }
}

/// Small rounded status pill whose right edge sits at `right`. Returns its rect.
pub fn pill(painter: &Painter, right: Pos2, text: &str, color: [u8; 3]) -> Rect {
    let font = theme::mono(10.5);
    let galley = painter.layout_no_wrap(text.to_string(), font, c(color));
    let size = galley.size() + vec2(14.0, 6.0);
    let rect = Rect::from_min_size(pos2(right.x - size.x, right.y - size.y / 2.0), size);
    painter.rect(rect, CornerRadius::same(6), ca(color, 22), Stroke::new(1.0, ca(color, 60)), StrokeKind::Inside);
    painter.galley(rect.min + vec2(7.0, 3.0), galley, c(color));
    rect
}

/// Ring glyph for the legend and lists.
#[derive(Clone, Copy)]
pub enum RingIcon {
    NeedsYou,
    Working,
    Resting,
    System,
    /// A resting ring that shrinks until the next scheduled wake (D23).
    Countdown,
}

pub fn ring_icon(painter: &Painter, center: Pos2, rx: f32, kind: RingIcon, time: f32) {
    let ry = rx * 0.34;
    let ellipse = |r: f32, from: f32, len: f32| -> Vec<Pos2> {
        let n = 32;
        (0..=n)
            .map(|k| from + len * k as f32 / n as f32)
            .map(|t| pos2(center.x + r * t.cos(), center.y - ry * (r / rx) * t.sin()))
            .collect()
    };
    match kind {
        RingIcon::NeedsYou => painter.add(Shape::line(ellipse(rx, 0.0, TAU), Stroke::new(2.0, c(theme::NEEDS_YOU)))),
        RingIcon::Working => {
            painter.add(Shape::line(ellipse(rx, 0.0, TAU), Stroke::new(2.0, ca(theme::WORKING, 60))));
            painter.add(Shape::line(ellipse(rx, time * 2.6, PI), Stroke::new(2.0, c(theme::WORKING))))
        }
        RingIcon::Resting => painter.add(Shape::line(ellipse(rx, 0.0, TAU), Stroke::new(2.0, c(theme::RESTING)))),
        RingIcon::Countdown => {
            painter.add(Shape::line(ellipse(rx, 0.0, TAU), Stroke::new(2.0, ca(theme::RESTING, 70))));
            let left = 1.0 - (time / 6.0).fract();
            painter.add(Shape::line(ellipse(rx, 0.0, TAU * left), Stroke::new(2.0, c(theme::TEXT_2))))
        }
        RingIcon::System => {
            painter.add(Shape::line(ellipse(rx, 0.0, TAU), Stroke::new(2.0, c(theme::SYSTEM))));
            painter.add(Shape::line(ellipse(rx * 0.62, 0.0, TAU), Stroke::new(1.6, c(theme::SYSTEM_2))))
        }
    };
}

/// The agent's rendered portrait, or a coloured disc while it has none.
pub fn portrait(painter: &Painter, rect: Rect, core: &Core, a: &Agent) {
    if let Some(tex) = core.portraits.get(&a.id) {
        let tint = if a.ring == RingState::Resting { Color32::from_gray(170) } else { Color32::WHITE };
        painter.image(tex, rect, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), tint);
    } else {
        painter.circle_filled(rect.center(), rect.width().min(rect.height()) * 0.3, c(a.color));
    }
}

/// Flat button drawn like the mockups. `primary` fills it with the signal colour.
pub fn button(ui: &mut Ui, rect: Rect, label: &str, style: ButtonStyle) -> Response {
    let resp = ui.interact(rect, ui.id().with(("button", label, rect.min.x as i32, rect.min.y as i32)), Sense::click());
    let hovered = resp.hovered();
    let (fill, stroke, text) = match style {
        ButtonStyle::Primary => (if hovered { c([0xA6, 0xDD, 0xFF]) } else { c(theme::NEEDS_YOU) }, Stroke::NONE, c([0x0B, 0x1A, 0x26])),
        ButtonStyle::Quiet => (
            if hovered { c([0x1A, 0x1F, 0x2A]) } else { Color32::TRANSPARENT },
            Stroke::new(1.0, if hovered { c([0x3A, 0x42, 0x52]) } else { c([0x2A, 0x30, 0x3D]) }),
            c([0xC9, 0xCE, 0xD9]),
        ),
        ButtonStyle::Selected => (c([0x1E, 0x23, 0x2E]), Stroke::NONE, c(theme::TEXT)),
        ButtonStyle::Ghost => (if hovered { c([0x1A, 0x1F, 0x2A]) } else { Color32::TRANSPARENT }, Stroke::NONE, c(theme::TEXT_2)),
    };
    let painter = ui.painter();
    painter.rect(rect, CornerRadius::same(10), fill, stroke, StrokeKind::Inside);
    let font = if style == ButtonStyle::Primary { theme::ui_bold(14.5) } else { theme::ui(14.5) };
    painter.text(rect.center(), Align2::CENTER_CENTER, label, font, text);
    if hovered {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ButtonStyle {
    Primary,
    Quiet,
    Selected,
    Ghost,
}

/// Panel background with the token border.
pub fn panel(painter: &Painter, rect: Rect, radius: u8) {
    painter.rect(rect, CornerRadius::same(radius), c(theme::PANEL), Stroke::new(1.0, c([0x20, 0x24, 0x2E])), StrokeKind::Inside);
}

/// Text cut to fit `width`, with an ellipsis.
pub fn fit(painter: &Painter, text: &str, font: FontId, width: f32) -> String {
    let full = painter.layout_no_wrap(text.to_string(), font.clone(), Color32::WHITE).size().x;
    if full <= width {
        return text.to_string();
    }
    let mut s: String = text.to_string();
    while !s.is_empty() {
        s.pop();
        let candidate = format!("{}…", s.trim_end());
        if painter.layout_no_wrap(candidate.clone(), font.clone(), Color32::WHITE).size().x <= width {
            return candidate;
        }
    }
    String::new()
}

/// The meerora mark, a person and a robot holding hands as an M
/// (assets/logo), in a square of `size` points around `center`.
pub fn logo(painter: &Painter, center: Pos2, size: f32) {
    // egui has no mipmaps here, so the picture closest to the drawn pixels keeps the edges smooth.
    const PICTURES: [(u32, &[u8]); 3] = [
        (32, include_bytes!("../../../../assets/logo/meerora-logo-32.png")),
        (64, include_bytes!("../../../../assets/logo/meerora-logo-64.png")),
        (128, include_bytes!("../../../../assets/logo/meerora-logo-128.png")),
    ];
    let pixels = size * painter.pixels_per_point();
    let (side, png) = PICTURES.iter().find(|(side, _)| pixels <= *side as f32 * 0.75).unwrap_or(&PICTURES[2]);
    let ctx = painter.ctx();
    let id = egui::Id::new(("meerora-logo", *side));
    let texture = ctx.data(|d| d.get_temp::<egui::TextureHandle>(id)).unwrap_or_else(|| {
        let image = image::load_from_memory(png).expect("the logo is a PNG").to_rgba8();
        let image = egui::ColorImage::from_rgba_unmultiplied([image.width() as usize, image.height() as usize], image.as_raw());
        let texture = ctx.load_texture(format!("meerora-logo-{side}"), image, egui::TextureOptions::LINEAR);
        ctx.data_mut(|d| d.insert_temp(id, texture.clone()));
        texture
    });
    let uv = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
    painter.image(texture.id(), Rect::from_center_size(center, vec2(size, size)), uv, Color32::WHITE);
}

fn draw_toasts(ui: &mut Ui, core: &mut Core) {
    if core.ui.toasts.is_empty() {
        return;
    }
    let screen = ui.ctx().content_rect();
    let width = 560.0;
    let mut y = 84.0;
    let painter = ui.ctx().layer_painter(egui::LayerId::new(egui::Order::Tooltip, egui::Id::new("toasts")));
    for t in &core.ui.toasts {
        let color = if t.error { [0xE0, 0x67, 0x5A] } else { theme::NEEDS_YOU };
        let galley = painter.layout(t.text.clone(), theme::mono(12.0), c(theme::TEXT), width - 32.0);
        let rect = Rect::from_min_size(pos2(screen.center().x - width / 2.0, y), vec2(width, galley.size().y + 20.0));
        painter.rect(rect, CornerRadius::same(10), c([0x1A, 0x14, 0x16]), Stroke::new(1.0, ca(color, 140)), StrokeKind::Inside);
        painter.galley(rect.min + vec2(16.0, 10.0), galley, c(theme::TEXT));
        y = rect.max.y + 8.0;
    }
}

fn draw_stats(ui: &mut Ui, core: &mut Core) {
    let painter = ui.ctx().layer_painter(egui::LayerId::new(egui::Order::Tooltip, egui::Id::new("stats")));
    let s = core.last_stats;
    let text = format!(
        "{:>5.1} fps\n{:>6} triangles\n{:>6} draw calls\n{} agents",
        core.ui.fps,
        s.triangles,
        s.draw_calls,
        core.world.agents.len()
    );
    let screen = ui.ctx().content_rect();
    let pos = pos2(screen.max.x - 190.0, screen.max.y - 92.0);
    let galley = painter.layout_no_wrap(text, theme::mono(11.5), c(theme::TEXT_2));
    let rect = Rect::from_min_size(pos - vec2(10.0, 8.0), galley.size() + vec2(20.0, 16.0));
    painter.rect_filled(rect, CornerRadius::same(8), ca([0x0B, 0x0D, 0x12], 220));
    painter.galley(pos, galley, c(theme::TEXT_2));
}

pub fn vline(painter: &Painter, x: f32, y0: f32, y1: f32) {
    painter.line_segment([pos2(x, y0), pos2(x, y1)], Stroke::new(1.0, c([0x1B, 0x1F, 0x28])));
}

pub fn hline(painter: &Painter, y: f32, x0: f32, x1: f32) {
    painter.line_segment([pos2(x0, y), pos2(x1, y)], Stroke::new(1.0, c([0x1B, 0x1F, 0x28])));
}

pub fn label(painter: &Painter, pos: Pos2, text: &str) -> Rect {
    painter.text(pos, Align2::LEFT_TOP, text, theme::mono(11.0), c(theme::TEXT_2))
}

#[cfg(test)]
mod tests {
    use super::*;
    use meerora_protocol::App;

    #[test]
    fn instance_handles_round_trip() {
        let tabs = [
            (AppTab::Chat, "chat"),
            (AppTab::Changes, "changes"),
            (AppTab::Activity, "activity"),
            (AppTab::Git(1), "git:1"),
            (AppTab::Git(3), "git:3"),
            (AppTab::Terminal(1), "terminal:1"),
            (AppTab::Terminal(12), "terminal:12"),
            (AppTab::Browser(2), "browser:2"),
            (AppTab::Editor(1), "editor:1"),
            (AppTab::Files(3), "files:3"),
            (AppTab::Viewer(2), "viewer:2"),
            (AppTab::Window(7), "window:7"),
        ];
        for (tab, handle) in tabs {
            assert_eq!(tab.handle(), handle);
            assert_eq!(tab.to_string(), handle);
            assert_eq!(AppTab::parse_handle(handle), Some(tab), "{handle}");
            assert_eq!(handle.parse::<AppTab>(), Ok(tab));
            let json = serde_json::to_string(&tab).unwrap();
            assert_eq!(serde_json::from_str::<AppTab>(&json).unwrap(), tab, "{json}");
        }
        assert_eq!(AppTab::parse_handle("terminal"), Some(AppTab::Terminal(1)), "no number is instance 1");
        assert_eq!(AppTab::parse_handle(" browser:2 "), Some(AppTab::Browser(2)));
        for bad in ["", "terminal:0", "terminal:", "terminal:x", "terminal:-1", "chat:1", "window", "window:", "shell:1", "Terminal:1"] {
            assert_eq!(AppTab::parse_handle(bad), None, "{bad:?}");
        }
        for app in App::ALL {
            let tab = AppTab::of(app, 2);
            assert_eq!(tab.app(), Some(app));
            assert_eq!(tab.instance(), if multi(app) { 2 } else { 1 });
            assert_eq!(AppTab::parse_handle(&tab.handle()), Some(tab));
        }
        assert_eq!(AppTab::Window(7).app(), None);
    }

    #[test]
    fn labels_and_keys_number_from_the_second_instance() {
        assert_eq!(AppTab::Terminal(1).label(), "Terminal");
        assert_eq!(AppTab::Terminal(2).label(), "Terminal 2");
        assert_eq!(AppTab::Browser(3).label(), "Browser 3");
        assert_eq!(AppTab::Chat.label(), "Chat");
        assert_eq!(instance_key("ember", 1), "ember");
        assert_eq!(instance_key("ember", 2), "ember#2");
        assert_eq!(serde_json::to_string(&AppTab::Terminal(1)).unwrap(), r#""terminal""#, "the spelling of old layouts");
        assert_eq!(serde_json::to_string(&AppTab::Terminal(2)).unwrap(), r#"{"terminal":2}"#);
        assert_eq!(serde_json::to_string(&AppTab::Window(7)).unwrap(), r#"{"window":7}"#);
    }
}
