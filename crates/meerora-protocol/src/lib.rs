//! Messages between the agent daemon and the meerora app (D16), plus the
//! `agent.toml` format of an agent directory (D21, D23).
//!
//! Transport is newline-delimited JSON over a Unix socket at [`socket_path`].
//! The daemon is the source of truth. Right after a client says hello it sends
//! the full current state as ordinary events, then streams changes.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

mod assets;
pub use assets::{INSTALLED_ASSETS, assets_dir, find_assets};
pub mod gitsafe;
mod rooms;
pub mod safefs;
pub mod workspace;
pub use rooms::{
    CUSTOM_ROOM, DEFAULT_ROOM, Room, SpaceSettings, TEMPLATES, Template, current_room, ensure_chosen_room, ensure_room, room_dir,
    room_name_ok, rooms, same_scripts, set_room, space_dir, templates_dir,
};

pub const PROTOCOL_VERSION: u32 = 3;

/// The ring above a figure (D3). The double violet ring for system agents is
/// not a state. It comes from [`Event::AgentSpawned::system`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RingState {
    Working,
    NeedsYou,
    Resting,
}

/// Daemon to app.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    Hello {
        version: u32,
        daemon: String,
    },
    /// The agent exists. `agent` is the name of its directory (D21).
    AgentSpawned {
        agent: String,
        name: String,
        #[serde(default)]
        system: bool,
    },
    AgentRemoved {
        agent: String,
    },
    Ring {
        agent: String,
        state: RingState,
        /// 0 to 1, shown next to a working ring.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        progress: Option<f32>,
    },
    /// Walk to a named station of the space (D9).
    GoTo {
        agent: String,
        station: String,
    },
    /// One line under the name, for example "fixing wifi".
    Activity {
        agent: String,
        text: String,
    },
    /// Sets a figure param declared by the figure script (D24).
    Param {
        agent: String,
        name: String,
        value: serde_json::Value,
    },
    /// An action on one of your apps, for the action log (D12).
    Action {
        agent: String,
        tool: String,
        /// One short line for the log, cut to 80 characters.
        summary: String,
        /// The full command, path, URL or query when `summary` had to be
        /// shortened. Apps that open what the agent touched use it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target: Option<String>,
    },
    /// Text the agent types into its terminal tile (D13).
    TerminalInput {
        agent: String,
        data: String,
    },
    /// A chat message in the agent panel. Sent again with the same `id`
    /// while the text grows, `done` marks the last version. On hello the
    /// daemon replays the recent conversation of every agent.
    Message {
        agent: String,
        id: String,
        role: Role,
        text: String,
        done: bool,
        /// Unix time in milliseconds when the message started.
        at: u64,
    },
    /// The agent started working on a task, from you or from its schedule (D23).
    RunStarted {
        agent: String,
        run: String,
        trigger: Trigger,
        task: String,
        /// Unix time in milliseconds when the run started, 0 when unknown.
        #[serde(default)]
        started: u64,
    },
    RunFinished {
        agent: String,
        run: String,
        outcome: Outcome,
        /// One line for the needs-you card and the action log.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary: Option<String>,
    },
    /// Progress of a builder session (screen 3). `script` is set once a
    /// version passed the validator.
    FigureDraft {
        session: String,
        status: DraftStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        script: Option<String>,
        /// What the builder says about this version, or the error.
        message: String,
        /// Triangles of the validated script.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        triangles: Option<usize>,
    },
    /// Answer to [`Command::Tool`], matched by `id`.
    ToolResult {
        id: String,
        ok: bool,
        message: String,
        /// A PNG as base64, the answer of `app_screenshot` (D30). The bridge
        /// gives it to the model as an image.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        image: Option<String>,
    },
    /// When a scheduled agent wakes next (D23), in unix milliseconds. The
    /// resting ring counts down from `since` to `at`. `at` is `None` when the
    /// agent has no schedule. Sent on hello and whenever the time changes.
    NextWake {
        agent: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        at: Option<u64>,
        since: u64,
    },
    /// An agent asks for a capability it does not hold (D8). Answered with
    /// [`Command::DecideCapability`]. Sent again on hello while undecided.
    CapabilityRequest {
        id: String,
        agent: String,
        capability: String,
        /// Why the agent wants it, in its own words.
        reason: String,
    },
    /// The capabilities of an agent changed, after a decision or a revoke.
    /// `granted` is the complete list afterwards.
    CapabilitiesChanged {
        agent: String,
        granted: Vec<String>,
    },
    /// Answer to [`Command::IndexSearch`], matched by `request`.
    IndexResults {
        request: String,
        #[serde(default)]
        results: Vec<IndexEntry>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// Answer to [`Command::ImportAgent`]. `agent` is the new directory name.
    Imported {
        manifest: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },

    // Personal agents and the feed (D26, D27, D28).
    /// The user's account on the index (D26, Q10). Sent on hello and after
    /// [`Command::CreateAccount`]. `handle` is `None` without an account.
    /// The token never leaves the daemon.
    Account {
        index: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        handle: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// Progress of an interview ([`Command::Interview`]). `question` is the
    /// next question, `profile` the validated draft so far. `done` is set
    /// when the interviewer has nothing more to ask.
    InterviewStep {
        session: String,
        status: DraftStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        question: Option<String>,
        /// Boxed, so every event stays small.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        profile: Option<Box<Profile>>,
        #[serde(default)]
        done: bool,
        /// What the interviewer says, or the error.
        message: String,
    },
    /// The profile of the personal agent as saved on this computer, private
    /// fields included. Sent on hello and after [`Command::SaveProfile`].
    /// `published` is the unix milliseconds of the last publish.
    OwnProfile {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        profile: Option<Box<Profile>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        published: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// Answer to [`Command::Social`]: followers, follows and friend requests
    /// after the action, matched by `request`.
    Social {
        request: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        relations: Option<Relations>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// One page of the feed, newest first (D27). Answers [`Command::ReadFeed`]
    /// by `request`. The daemon also pushes the first page with an empty
    /// `request` after a post went out or a relation changed.
    Feed {
        request: String,
        #[serde(default)]
        posts: Vec<Post>,
        /// Cursor for the next older page.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        next: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// Every post draft that waits for the owner's approval (D26), the
    /// complete list. Sent on hello and whenever it changes. `published` is
    /// the post that just went out, when that was the change.
    PostDrafts {
        #[serde(default)]
        drafts: Vec<PostDraft>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        published: Option<Post>,
    },
    /// The news agent's latest digest (D28), pinned at the top of the feed.
    /// Sent on hello and whenever it or a reaction changes.
    NewsDigest {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        digest: Option<Digest>,
    },
    /// Answer to [`Command::AddTemplate`]. `agent` is the new directory name.
    TemplateAdded {
        template: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },

    // Devices (D7, D8).
    /// Every device script the daemon found, the complete list. Sent on
    /// hello and whenever it changes. A device with `error` did not load.
    Devices {
        #[serde(default)]
        devices: Vec<DeviceInfo>,
    },
    /// A device param follows its state source (D7). Sent on hello for every
    /// bound param and whenever the state changes, whoever changed it.
    DeviceParam {
        device: String,
        name: String,
        value: serde_json::Value,
    },
    /// A device of the space needs the user's approval for the capability of
    /// an action (D8). Answered with [`Command::DecideCapability`], like the
    /// request of an agent. Sent again on hello while undecided.
    DeviceCapabilityRequest {
        id: String,
        device: String,
        capability: String,
        /// The label of the action that waits for the decision.
        action: String,
    },
    /// The capabilities of a device of the space changed, after a decision
    /// or a revoke. `granted` is the complete list afterwards.
    DeviceCapabilitiesChanged {
        device: String,
        granted: Vec<String>,
    },
    /// Answer to [`Command::DeviceAction`].
    DeviceActionResult {
        device: String,
        action: String,
        ok: bool,
        message: String,
    },

    // Graveyard.
    /// Every agent in the graveyard, newest first, the complete list. Sent
    /// on hello and whenever it changes. A retired agent has left the space,
    /// and its directory waits below [`GRAVEYARD_DIR`] until it comes back.
    Graveyard {
        #[serde(default)]
        agents: Vec<RetiredAgent>,
    },
    /// Answer to [`Command::Retire`], [`Command::Restore`] and
    /// [`Command::DeleteRetired`], sent only to the client that asked.
    /// `agent` repeats the id of the command. `id` is where the agent went:
    /// its graveyard id after a retire, its agent id after a restore. `error`
    /// says in plain words why nothing happened.
    GraveyardResult {
        action: GraveyardAction,
        agent: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },

    // The command box of the app (Alt+Space).
    /// The answer to [`Command::Ask`], sent only to the client that asked.
    /// Sent again with the same `request` while the text grows, block by
    /// block. `done` marks the last one, which carries `error` when the
    /// question failed or was stopped.
    Answer {
        request: String,
        text: String,
        done: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },

    // Agent and app standard (D30).
    /// An app tool call of an agent, which the daemon checked against the
    /// agent's capabilities. Sent only to the app client, the most recent
    /// client that sent [`Command::ServeApps`] and is still connected. The
    /// app answers with [`Command::AppReply`] and the same `id`. `granted`
    /// is what the agent holds now, so the app can decide details such as
    /// the network of an app it starts (D29).
    AppRequest {
        id: String,
        agent: String,
        request: AppCall,
        #[serde(default)]
        granted: Vec<String>,
    },

    // The helper (`role = "helper"`).
    /// The helper proposes a change of meerora's configuration
    /// (`config_change`). The app shows the diff with Apply and Discard and
    /// answers with [`Command::DecideConfig`]. Sent again on hello while
    /// undecided.
    ConfigProposal {
        id: String,
        agent: String,
        /// `meerora.toml`, `commands.toml`, `agent:<id>` or `new_agent:<id>`.
        target: String,
        /// The file it writes, relative to the data dir.
        path: String,
        /// Why, in the helper's words.
        reason: String,
        /// A unified diff of the files as they are and as they would be.
        /// Hunk heads of TOML files name the table they start in, like
        /// `@@ -4,2 +4,3 @@ [capabilities]`.
        diff: String,
    },
    /// A proposal was decided or closed. `applied` says whether the files
    /// were written. `message` says what happened in plain words.
    ConfigResult {
        id: String,
        applied: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Agent,
    /// Notes from meerora itself, for example "claude is not installed".
    System,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Trigger {
    User,
    Schedule,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// Finished, the agent rests.
    Done,
    /// Finished and waits for you (D3).
    NeedsYou,
    Failed,
    Cancelled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DraftStatus {
    Working,
    Ready,
    Failed,
}

/// A meerora tool an agent called through the MCP bridge (D22, D24).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "tool", rename_all = "snake_case")]
pub enum ToolCall {
    /// Sets a param of the agent's own figure (D24).
    FigureSet {
        name: String,
        value: serde_json::Value,
    },
    GoTo {
        station: String,
    },
    SetActivity {
        text: String,
    },
    /// Ends the run waiting for the user, with the question as summary (D3).
    AskUser {
        question: String,
    },
    /// A short progress note in the agent panel.
    Note {
        text: String,
    },
    /// Asks the user for a capability (D8), for example `web` or
    /// `network.airplane_mode.write`. The tool call returns once the user
    /// decided.
    RequestCapability {
        capability: String,
        reason: String,
    },

    // Personal agents and the feed (D26, D27, D28).
    /// A post about the owner, from the personal agent. It waits for the
    /// owner's approval and never goes out without it (D26).
    DraftPost {
        text: String,
        visibility: Visibility,
    },
    /// The newest posts of the feed as text (D27), for the personal and the news agent.
    ReadFeed {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<u32>,
    },
    /// The news agent's digest (D28), kept on this computer and pinned at the top of the feed.
    PostDigest {
        title: String,
        items: Vec<DigestItem>,
    },

    // The helper (`role = "helper"`).
    /// meerora's configuration as text: `meerora.toml`, `agents`,
    /// `agent:<id>`, `rooms`, `commands`, `audit` or `check`. Secrets such as
    /// the index token and the CLI homes in `runtimes/` never leave.
    ConfigRead {
        target: String,
    },
    /// Proposes the whole new content of `meerora.toml`, `commands.toml`,
    /// `agent:<id>` (its agent.toml) or `new_agent:<id>` (an agent.toml,
    /// with `figure` as its figure script). The daemon checks it and shows
    /// the user a diff with [`Event::ConfigProposal`]. The tool call returns
    /// once the user applied or discarded it.
    ConfigChange {
        target: String,
        content: String,
        reason: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        figure: Option<String>,
    },

    // Agent and app standard (D30, docs/design/agent-apps.md). The daemon
    // checks the capability and hands them to the app as `Event::AppRequest`.
    /// Every open app instance of the agent's view (`apps_list`), needs `apps.read`.
    AppsList,
    /// What an app instance shows as text, with the elements to act on
    /// (`app_read`), needs `apps.read`. `query` keeps only the matching lines and elements.
    AppRead {
        app: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        query: Option<String>,
    },
    /// A PNG of an app instance as the user sees it (`app_screenshot`), needs `apps.read`.
    AppScreenshot {
        app: String,
    },
    /// One action of the instance's vocabulary (`app_act`), such as `run`
    /// in a terminal. Needs `apps.control`, or `apps.input` for `window:*`.
    AppAct {
        app: String,
        action: String,
        #[serde(default)]
        args: serde_json::Value,
    },
    /// Opens a built-in app (`apps.control`) or starts a real app by its
    /// desktop id or program (`apps.launch`), `app_open`.
    AppOpen {
        app: String,
        #[serde(default)]
        args: serde_json::Value,
    },
}

/// An app tool call as the app receives it in [`Event::AppRequest`] (D30).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "call", rename_all = "snake_case")]
pub enum AppCall {
    List,
    Read {
        app: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        query: Option<String>,
    },
    Screenshot {
        app: String,
    },
    Act {
        app: String,
        action: String,
        #[serde(default)]
        args: serde_json::Value,
    },
    Open {
        app: String,
        #[serde(default)]
        args: serde_json::Value,
    },
}

/// The kinds of built-in apps an agent may open with `app_open` (D30).
/// Everything else names a real app.
pub const BUILT_IN_APPS: [&str; 9] = ["chat", "terminal", "browser", "editor", "files", "viewer", "changes", "activity", "git"];

impl AppCall {
    /// The app call of a tool call, None for the other tools.
    pub fn from_tool(call: &ToolCall) -> Option<AppCall> {
        Some(match call {
            ToolCall::AppsList => AppCall::List,
            ToolCall::AppRead { app, query } => AppCall::Read { app: app.clone(), query: query.clone() },
            ToolCall::AppScreenshot { app } => AppCall::Screenshot { app: app.clone() },
            ToolCall::AppAct { app, action, args } => AppCall::Act { app: app.clone(), action: action.clone(), args: args.clone() },
            ToolCall::AppOpen { app, args } => AppCall::Open { app: app.clone(), args: args.clone() },
            _ => return None,
        })
    }

    /// The MCP tool name, for messages and the audit log.
    pub fn tool(&self) -> &'static str {
        match self {
            AppCall::List => "apps_list",
            AppCall::Read { .. } => "app_read",
            AppCall::Screenshot { .. } => "app_screenshot",
            AppCall::Act { .. } => "app_act",
            AppCall::Open { .. } => "app_open",
        }
    }

    /// The handle or app it is about, None for `apps_list`.
    pub fn app(&self) -> Option<&str> {
        match self {
            AppCall::List => None,
            AppCall::Read { app, .. } | AppCall::Screenshot { app } | AppCall::Act { app, .. } | AppCall::Open { app, .. } => Some(app),
        }
    }

    /// The capability it needs (D30): `apps.read` to look, `apps.control` to
    /// act on and open built-in apps, `apps.input` to act on real app
    /// windows and `apps.launch` to start real apps.
    pub fn capability(&self) -> &'static str {
        match self {
            AppCall::List | AppCall::Read { .. } | AppCall::Screenshot { .. } => "apps.read",
            AppCall::Act { app, .. } if app.trim().starts_with("window:") => "apps.input",
            AppCall::Act { .. } => "apps.control",
            AppCall::Open { app, .. } if BUILT_IN_APPS.contains(&app.trim()) => "apps.control",
            AppCall::Open { .. } => "apps.launch",
        }
    }
}

/// The app's answer to an [`Event::AppRequest`] (D30): the text for the
/// model or the error, and a PNG for `app_screenshot`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AppResult {
    pub ok: bool,
    /// JSON for `apps_list` and `app_read`, a sentence for `app_act` and
    /// `app_open`, the error in plain words when not `ok`.
    pub text: String,
    /// A PNG as base64.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// The entry for the agent's action log after a successful act or open
    /// (D12). The daemon sends it as [`Event::Action`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log: Option<AppLog>,
}

impl AppResult {
    pub fn text(text: impl Into<String>) -> Self {
        Self { ok: true, text: text.into(), image: None, log: None }
    }

    pub fn error(text: impl Into<String>) -> Self {
        Self { ok: false, text: text.into(), image: None, log: None }
    }
}

/// One line of the action log: `tool` is `<kind>.<action>`, for example
/// `terminal.run` or `browser.click`, and `summary` says what, for example
/// `cargo test` or `Sign in`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AppLog {
    pub tool: String,
    pub summary: String,
}

impl Event {
    pub fn agent(&self) -> Option<&str> {
        match self {
            Event::Hello { .. } => None,
            Event::AgentSpawned { agent, .. }
            | Event::AgentRemoved { agent }
            | Event::Ring { agent, .. }
            | Event::GoTo { agent, .. }
            | Event::Activity { agent, .. }
            | Event::Param { agent, .. }
            | Event::Action { agent, .. }
            | Event::TerminalInput { agent, .. }
            | Event::Message { agent, .. }
            | Event::RunStarted { agent, .. }
            | Event::RunFinished { agent, .. }
            | Event::NextWake { agent, .. }
            | Event::CapabilityRequest { agent, .. }
            | Event::CapabilitiesChanged { agent, .. }
            | Event::AppRequest { agent, .. }
            | Event::ConfigProposal { agent, .. } => Some(agent),
            Event::FigureDraft { .. } | Event::ToolResult { .. } | Event::IndexResults { .. } | Event::Imported { .. } => None,
            Event::Account { .. }
            | Event::InterviewStep { .. }
            | Event::OwnProfile { .. }
            | Event::Social { .. }
            | Event::Feed { .. }
            | Event::PostDrafts { .. }
            | Event::NewsDigest { .. }
            | Event::TemplateAdded { .. } => None,
            Event::Devices { .. }
            | Event::DeviceParam { .. }
            | Event::DeviceCapabilityRequest { .. }
            | Event::DeviceCapabilitiesChanged { .. }
            | Event::DeviceActionResult { .. } => None,
            Event::Graveyard { .. } | Event::GraveyardResult { .. } => None,
            Event::Answer { .. } => None,
            Event::ConfigResult { .. } => None,
        }
    }
}

/// App (or the `meerora-agentd send` tool) to daemon.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Command {
    Hello {
        version: u32,
        client: String,
    },
    /// Broadcast this event to every client as if the daemon had produced it.
    Inject {
        event: Event,
    },
    /// The user attended to an agent that needed them.
    Ack {
        agent: String,
    },
    /// A chat message from the user. Starts a run, or continues the
    /// conversation of the running one.
    Say {
        agent: String,
        text: String,
    },
    /// Stops the agent's current run.
    Cancel {
        agent: String,
    },
    /// Agent directories changed on disk, for example after the builder created one.
    ReloadAgents,
    /// Asks the builder for a figure script (screen 3). `script` is the
    /// current version when the user asks for a change.
    Build {
        session: String,
        prompt: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        script: Option<String>,
    },
    /// From the MCP bridge of `agent`. Answered with [`Event::ToolResult`].
    Tool {
        id: String,
        agent: String,
        call: ToolCall,
    },
    /// Decides a [`Event::CapabilityRequest`]. With `remember` the grant is
    /// written to `agent.toml`, else it holds for the current run only.
    DecideCapability {
        id: String,
        approve: bool,
        #[serde(default)]
        remember: bool,
    },
    /// Takes a granted capability away from an agent.
    RevokeCapability {
        agent: String,
        capability: String,
    },
    /// Searches the meerora index (D25, D26). Answered with [`Event::IndexResults`].
    IndexSearch {
        request: String,
        query: String,
    },
    /// Imports an external agent by manifest id (D25). Answered with [`Event::Imported`].
    ImportAgent {
        manifest: String,
    },

    // Personal agents and the feed (D26, D27, D28).
    /// Creates the user's account on the index (D26). Answered with [`Event::Account`].
    CreateAccount {
        handle: String,
    },
    /// Asks the interviewer for the next question and the profile so far
    /// (D26). `agent` is the personal agent, whose runtime asks. `turns` is
    /// the interview so far, the last answer included. Answered with
    /// [`Event::InterviewStep`].
    Interview {
        session: String,
        agent: String,
        #[serde(default)]
        turns: Vec<InterviewTurn>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        profile: Option<Profile>,
    },
    /// Saves the personal profile on this computer and, with `publish`,
    /// pushes it to the index without its private fields. Answered with
    /// [`Event::OwnProfile`].
    SaveProfile {
        profile: Profile,
        #[serde(default)]
        publish: bool,
    },
    /// Follow, befriend and friend requests (D27). Answered with [`Event::Social`].
    Social {
        request: String,
        action: SocialAction,
    },
    /// Reads one page of the feed, the newest when `before` is unset.
    /// Answered with [`Event::Feed`].
    ReadFeed {
        request: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        before: Option<String>,
    },
    /// The owner decides a post draft (D26). An approved draft goes out with
    /// `text` and `visibility` when given, so edits made while reviewing count.
    DecidePost {
        id: String,
        approve: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        visibility: Option<Visibility>,
    },
    /// A reaction to an item of the news digest (D28). The daemon appends it
    /// to the news agent's `memory/reactions.jsonl`.
    React {
        digest: String,
        item: String,
        reaction: Reaction,
    },
    /// Adds an agent from a template, so far only `news` (D28). Answered with
    /// [`Event::TemplateAdded`].
    AddTemplate {
        template: String,
    },

    // Devices (D7, D8).
    /// The user clicked an action on a device card. Answered with
    /// [`Event::DeviceActionResult`]. An action of a space device whose
    /// capability is not approved yet sends [`Event::DeviceCapabilityRequest`]
    /// first and runs once the user allows it.
    DeviceAction {
        device: String,
        action: String,
    },
    /// Takes a capability away from a device of the space.
    RevokeDeviceCapability {
        device: String,
        capability: String,
    },

    // Agent view apps (D11, D23).
    /// Starts the task of one wake-up of the agent now, from the Activity
    /// app. `schedule` is its index in the `[[schedule]]` list of
    /// `agent.toml`. The daemon starts it like a due wake, so it records the
    /// wake and restarts the countdown. The token budget holds it back and a
    /// busy agent starts nothing. Quiet hours do not hold it back, since you
    /// asked.
    RunNow {
        agent: String,
        schedule: usize,
    },

    // Graveyard.
    /// Retires an agent reversibly. The daemon stops its run, its queue and
    /// its schedule, takes it out of the space and moves its directory to
    /// `graveyard/<id>/` with git, memory and logs. Its CLI homes stay, so a
    /// restore with the same id continues its conversations. Answered with
    /// [`Event::GraveyardResult`].
    Retire {
        agent: String,
    },
    /// Brings a retired agent back to `agents/`. `agent` is its id in the
    /// graveyard. Answered with [`Event::GraveyardResult`].
    Restore {
        agent: String,
    },
    /// Moves a retired agent's directory to the system trash with `gio
    /// trash`. Nothing is ever deleted for good. `agent` is its id in the
    /// graveyard. Answered with [`Event::GraveyardResult`].
    DeleteRetired {
        agent: String,
    },
    /// An agent's figure reached the station of its last [`Event::GoTo`] in
    /// the app. A retired agent leaves the space once it reached the
    /// graveyard (graveyard.rs of the daemon). Daemons that wait for nothing
    /// ignore it.
    Arrived {
        agent: String,
        station: String,
    },

    // Devices.
    /// The user picked a value of a device setting on its card, such as the
    /// metric of the system graph. The daemon keeps it in
    /// `devices/settings.toml` of the room and sends it back as
    /// [`Event::DeviceParam`], with the values of the bindings that follow it.
    DeviceSetting {
        device: String,
        name: String,
        value: serde_json::Value,
    },

    // The command box of the app (Alt+Space).
    /// A one-shot question (`?? question`). The daemon asks `agent`'s
    /// runtime once, outside its runs: no conversation, no meerora tools,
    /// read-only, with the agent's CLI home and sandbox. Answered with
    /// [`Event::Answer`] to this client only. Nothing is stored.
    Ask {
        request: String,
        agent: String,
        question: String,
    },
    /// Stops the question `request` of [`Command::Ask`]. Its last
    /// [`Event::Answer`] says it was stopped.
    CancelAsk {
        request: String,
    },

    // Agent and app standard (D30).
    /// This client holds the agent views and answers [`Event::AppRequest`].
    /// The app sends it right after its hello. The daemon sends app requests
    /// to the most recent client that sent it and is still connected, never
    /// to a bridge.
    ServeApps,
    /// The answer to [`Event::AppRequest`] `id`.
    AppReply {
        id: String,
        result: AppResult,
    },

    // The helper (`role = "helper"`).
    /// Decides an [`Event::ConfigProposal`]. With `apply` the daemon writes
    /// the files, reloads them and logs it in `audit.jsonl`. There is no
    /// "always" for configuration changes. Answered with [`Event::ConfigResult`].
    DecideConfig {
        id: String,
        apply: bool,
    },
}

/// What kind of owner publishes a manifest (D25, D26).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManifestKind {
    Organisation,
    Person,
}

/// An external agent manifest in the meerora index (D25, section 5 of the brief).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    /// Format version, "0.1".
    pub meerora_manifest: String,
    /// `<owner domain or handle>/<name>`, for example `meridian.example/mira`.
    pub id: String,
    pub kind: ManifestKind,
    pub name: String,
    pub owner: ManifestOwner,
    /// URL of the picture shown in search results.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub picture: Option<String>,
    pub figure: ManifestFigure,
    pub welcome: String,
    /// At most 5 (D25).
    #[serde(default)]
    pub search_terms: Vec<String>,
    pub endpoint: ManifestEndpoint,
    /// URL of the A2A agent card (Q9).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub a2a_card: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub privacy: Option<String>,
    #[serde(default)]
    pub languages: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ManifestOwner {
    pub name: String,
    /// Verified for organisations (D25). People use their index handle.
    pub domain: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ManifestFigure {
    /// URL of the figure script (D20).
    pub url: String,
    /// Hex SHA-256 of the script. Imports fail when it does not match.
    pub sha256: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ManifestEndpoint {
    pub url: String,
    /// "a2a/1.0" (Q9).
    pub protocol: String,
}

/// One search result of the meerora index, what the search screen shows (D25).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct IndexEntry {
    pub manifest: Manifest,
    /// The owner proved control of `manifest.owner.domain` (D25).
    pub verified: bool,
    /// The figure script, already checked against `manifest.figure.sha256`,
    /// for the live preview. `None` when it could not be fetched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub figure_script: Option<String>,
    /// The profile of a person (D26), filtered for whoever asked: public
    /// fields for everyone, friends fields for friends.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<Profile>,
}

// Personal agents and the feed (D26, D27, D28).

/// Who may see a profile field or a post (D26). Ordered from open to closed,
/// so `field.visibility <= allowed` keeps what a viewer may see.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Visibility {
    Public,
    Friends,
    /// Stays on this computer. The index never stores or returns it.
    Private,
}

/// The personal profile format of section 5 of the brief (D26). Every field
/// carries its own visibility.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    /// Format version, "0.1".
    #[serde(default = "profile_format")]
    pub meerora_profile: String,
    /// The index handle. Empty until the user has an account.
    #[serde(default)]
    pub handle: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<ProfileText>,
    /// Absolute URLs once published. On this computer a photo may also be a
    /// path relative to the personal agent's directory, such as `photos/1.jpg`.
    #[serde(default)]
    pub photos: Vec<ProfilePhoto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headline: Option<ProfileText>,
    #[serde(default)]
    pub education: Vec<Education>,
    #[serde(default)]
    pub work: Vec<Work>,
    #[serde(default)]
    pub places: Vec<ProfileText>,
    #[serde(default)]
    pub interests: Vec<ProfileText>,
    #[serde(default)]
    pub links: Vec<ProfileLink>,
}

/// The profile format version this build reads and writes.
pub const PROFILE_FORMAT: &str = "0.1";

fn profile_format() -> String {
    PROFILE_FORMAT.into()
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileText {
    pub value: String,
    pub visibility: Visibility,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfilePhoto {
    pub url: String,
    pub visibility: Visibility,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Education {
    pub school: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub degree: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<i32>,
    pub visibility: Visibility,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Work {
    pub org: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<i32>,
    pub visibility: Visibility,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileLink {
    pub url: String,
    pub visibility: Visibility,
}

/// Limits of a profile, the same for the interview, the app and the index.
pub mod profile_limits {
    pub const LIST: usize = 12;
    pub const INTERESTS: usize = 24;
    pub const NAME: usize = 80;
    pub const HEADLINE: usize = 160;
    pub const TEXT: usize = 120;
    pub const URL: usize = 300;
    pub const YEARS: std::ops::RangeInclusive<i32> = 1900..=2100;
}

impl Profile {
    /// The profile as a viewer allowed to see `allowed` sees it.
    pub fn visible(&self, allowed: Visibility) -> Profile {
        let ok = |v: Visibility| v <= allowed;
        Profile {
            meerora_profile: self.meerora_profile.clone(),
            handle: self.handle.clone(),
            name: self.name.clone().filter(|f| ok(f.visibility)),
            photos: self.photos.iter().filter(|p| ok(p.visibility)).cloned().collect(),
            headline: self.headline.clone().filter(|f| ok(f.visibility)),
            education: self.education.iter().filter(|e| ok(e.visibility)).cloned().collect(),
            work: self.work.iter().filter(|w| ok(w.visibility)).cloned().collect(),
            places: self.places.iter().filter(|p| ok(p.visibility)).cloned().collect(),
            interests: self.interests.iter().filter(|i| ok(i.visibility)).cloned().collect(),
            links: self.links.iter().filter(|l| ok(l.visibility)).cloned().collect(),
        }
    }

    /// True when nothing but the handle is filled in.
    pub fn is_empty(&self) -> bool {
        self.name.is_none()
            && self.headline.is_none()
            && self.photos.is_empty()
            && self.education.is_empty()
            && self.work.is_empty()
            && self.places.is_empty()
            && self.interests.is_empty()
            && self.links.is_empty()
    }

    /// Every problem of the profile, or nothing. The interview sends them
    /// back to the model, the index answers with them.
    pub fn problems(&self) -> Vec<String> {
        use profile_limits::*;
        let mut problems = Vec::new();
        let mut text = |what: &str, value: &str, max: usize, required: bool| {
            if value.trim().is_empty() {
                if required {
                    problems.push(format!("{what} is empty"));
                }
            } else if value.chars().count() > max {
                problems.push(format!("{what} is longer than {max} characters"));
            } else if value.contains(['\n', '\r']) {
                problems.push(format!("{what} must be one line"));
            }
        };
        if let Some(name) = &self.name {
            text("name.value", &name.value, NAME, true);
        }
        if let Some(headline) = &self.headline {
            text("headline.value", &headline.value, HEADLINE, true);
        }
        for (i, e) in self.education.iter().enumerate() {
            text(&format!("education[{i}].school"), &e.school, TEXT, true);
            text(&format!("education[{i}].degree"), e.degree.as_deref().unwrap_or(""), TEXT, false);
        }
        for (i, w) in self.work.iter().enumerate() {
            text(&format!("work[{i}].org"), &w.org, TEXT, true);
            text(&format!("work[{i}].role"), w.role.as_deref().unwrap_or(""), TEXT, false);
        }
        for (i, p) in self.places.iter().enumerate() {
            text(&format!("places[{i}].value"), &p.value, NAME, true);
        }
        for (i, p) in self.interests.iter().enumerate() {
            text(&format!("interests[{i}].value"), &p.value, NAME, true);
        }
        for (i, p) in self.photos.iter().enumerate() {
            text(&format!("photos[{i}].url"), &p.url, URL, true);
        }
        for (i, l) in self.links.iter().enumerate() {
            text(&format!("links[{i}].url"), &l.url, URL, true);
        }
        if self.meerora_profile != PROFILE_FORMAT {
            problems.push(format!("meerora_profile must be {PROFILE_FORMAT:?}, not {:?}", self.meerora_profile));
        }
        for (what, n, max) in [
            ("photos", self.photos.len(), LIST),
            ("education", self.education.len(), LIST),
            ("work", self.work.len(), LIST),
            ("places", self.places.len(), LIST),
            ("interests", self.interests.len(), INTERESTS),
            ("links", self.links.len(), LIST),
        ] {
            if n > max {
                problems.push(format!("{what} has {n} entries, at most {max} are allowed"));
            }
        }
        let years = self.education.iter().map(|e| ("education", e.from, e.to)).chain(self.work.iter().map(|w| ("work", w.from, w.to)));
        for (what, from, to) in years {
            for year in [from, to].into_iter().flatten() {
                if !YEARS.contains(&year) {
                    problems.push(format!("{what} has the year {year}, which is not between 1900 and 2100"));
                }
            }
            if let (Some(from), Some(to)) = (from, to)
                && from > to
            {
                problems.push(format!("{what} goes from {from} to {to}, which runs backwards"));
            }
        }
        for (i, l) in self.links.iter().enumerate() {
            if !(l.url.starts_with("https://") || l.url.starts_with("http://")) {
                problems.push(format!("links[{i}].url must start with https://"));
            }
        }
        problems
    }
}

/// Someone who posts or is followed (an ActivityPub actor, D27): a person by
/// index handle, or an organisation's agent by manifest id.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Actor {
    /// A handle such as `alex`, or a manifest id such as `meridian.example/mira`.
    pub id: String,
    pub kind: ManifestKind,
    /// The public name, or the handle when the name is not public.
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub picture: Option<String>,
    /// The organisation behind an agent, for example "Meridian Software".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
}

/// A post in the feed (an ActivityPub note, D27).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Post {
    pub id: String,
    pub author: Actor,
    pub text: String,
    /// Public or friends. Posts are never private.
    pub visibility: Visibility,
    /// Unix milliseconds.
    pub published: u64,
}

/// One follow seen from the user (D27). A friendship is two accepted
/// follows with `friend` set, one in each direction.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Relation {
    pub actor: Actor,
    #[serde(default)]
    pub friend: bool,
    /// Unix milliseconds of the follow or the request.
    pub since: u64,
}

/// The user's side of the social graph (D27).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Relations {
    /// Actors the user follows, friends included.
    #[serde(default)]
    pub follows: Vec<Relation>,
    /// People who follow the user, friends included.
    #[serde(default)]
    pub followers: Vec<Relation>,
    /// Friend requests to the user that wait for Accept or Decline.
    #[serde(default)]
    pub requests: Vec<Relation>,
    /// The user's friend requests that wait for the other side.
    #[serde(default)]
    pub pending: Vec<Relation>,
}

impl Relations {
    pub fn follows(&self, actor: &str) -> bool {
        self.follows.iter().any(|r| r.actor.id == actor)
    }

    pub fn is_friend(&self, handle: &str) -> bool {
        self.follows.iter().any(|r| r.actor.id == handle && r.friend) && self.followers.iter().any(|r| r.actor.id == handle && r.friend)
    }

    pub fn asked(&self, handle: &str) -> bool {
        self.pending.iter().any(|r| r.actor.id == handle)
    }
}

/// What [`Command::Social`] does (D27).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum SocialAction {
    /// Only reads the relations.
    Refresh,
    /// Follows a person (handle) or an organisation's agent (manifest id). One-way, no approval.
    Follow {
        actor: String,
    },
    Unfollow {
        actor: String,
    },
    /// Asks a person to be friends. Accepting makes two friend follows.
    Befriend {
        handle: String,
    },
    Accept {
        handle: String,
    },
    Decline {
        handle: String,
    },
}

/// A post the personal agent wrote. It waits for the owner (D26).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PostDraft {
    pub id: String,
    /// The agent that wrote it.
    pub agent: String,
    pub text: String,
    pub visibility: Visibility,
    /// Unix milliseconds.
    pub created: u64,
    /// Why the last approval did not go out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The news agent's digest (D28).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Digest {
    pub id: String,
    pub agent: String,
    pub title: String,
    /// Unix milliseconds.
    pub created: u64,
    pub items: Vec<DigestItem>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DigestItem {
    /// Given by the daemon.
    #[serde(default)]
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Where it comes from, for example a site or "your feed".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// More or less like this, set by the user.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reaction: Option<Reaction>,
    #[serde(default)]
    pub opened: bool,
}

/// How the user reacted to a digest item (D28). `skipped` is recorded for
/// items nobody opened or rated when the next digest arrives.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reaction {
    More,
    Less,
    Opened,
    Skipped,
}

/// One question of the interview and the user's answer (D26).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InterviewTurn {
    pub question: String,
    pub answer: String,
}

// Devices (D7, D8).

/// A device as the daemon loaded it (D7).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DeviceInfo {
    /// The file name without `.js` for a device of the space, `<agent>/<file
    /// name>` for a device an agent made.
    pub id: String,
    pub name: String,
    /// The agent that made it (D8). `None` for a device of the space, which
    /// belongs to the user.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    /// The script, relative to the data dir.
    pub script: String,
    /// Changes whenever the script changes, so the app builds the shape again.
    #[serde(default)]
    pub revision: String,
    #[serde(default)]
    pub actions: Vec<DeviceActionInfo>,
    /// Every capability the device names, for its actions and for reading
    /// its sources (D8).
    #[serde(default)]
    pub needs: Vec<String>,
    /// What it holds: the approvals of a space device, the grants of its
    /// agent for an agent's device. Shown as a badge on the object (D8).
    #[serde(default)]
    pub granted: Vec<String>,
    /// Why the device did not load. The app does not show it then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// An action of a device card (D7).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DeviceActionInfo {
    pub id: String,
    pub label: String,
    /// The declared capability it uses (D8).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capability: Option<String>,
    /// A view of the app it opens instead, such as `feed`. The app handles
    /// it without the daemon.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opens: Option<String>,
}

/// A device script on disk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceFile {
    /// See [`DeviceInfo::id`].
    pub id: String,
    pub owner: Option<String>,
    pub path: PathBuf,
}

/// Every device script below `data_dir`, sorted by id. The devices of the
/// space are `devices/*.js` in the folder of the room (the proposal of Q5,
/// see [`space_dir`]), those of agents are `agents/<id>/devices/*.js` in
/// agent directories with an `agent.toml` (D21).
pub fn device_files(data_dir: &Path) -> Vec<DeviceFile> {
    let scripts = |dir: &Path| -> Vec<(String, PathBuf)> {
        let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
        entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "js") && p.is_file())
            .filter_map(|p| Some((p.file_stem()?.to_str()?.to_string(), p)))
            .collect()
    };
    let mut files: Vec<DeviceFile> =
        scripts(&space_dir(data_dir).join("devices")).into_iter().map(|(id, path)| DeviceFile { id, owner: None, path }).collect();
    if let Ok(entries) = std::fs::read_dir(data_dir.join("agents")) {
        for dir in entries.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.join("agent.toml").is_file()) {
            let Some(agent) = dir.file_name().and_then(|n| n.to_str()).map(String::from) else { continue };
            for (name, path) in scripts(&dir.join("devices")) {
                files.push(DeviceFile { id: format!("{agent}/{name}"), owner: Some(agent.clone()), path });
            }
        }
    }
    files.sort_by(|a, b| a.id.cmp(&b.id));
    files
}

/// One JSON line including the trailing newline.
pub fn encode<T: Serialize>(msg: &T) -> String {
    let mut line = serde_json::to_string(msg).expect("protocol messages always serialize");
    line.push('\n');
    line
}

pub fn decode_event(line: &str) -> Result<Event, serde_json::Error> {
    serde_json::from_str(line.trim_end())
}

pub fn decode_command(line: &str) -> Result<Command, serde_json::Error> {
    serde_json::from_str(line.trim_end())
}

/// `$MEERORA_SOCKET`, else `$XDG_RUNTIME_DIR/meerora/agentd.sock`, else
/// `/tmp/meerora-$USER/agentd.sock`.
pub fn socket_path() -> PathBuf {
    if let Some(p) = std::env::var_os("MEERORA_SOCKET") {
        return PathBuf::from(p);
    }
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        return Path::new(&dir).join("meerora").join("agentd.sock");
    }
    let user = std::env::var("USER").unwrap_or_else(|_| "user".into());
    PathBuf::from(format!("/tmp/meerora-{user}/agentd.sock"))
}

/// `$MEERORA_DATA_DIR` if set. Otherwise `$XDG_DATA_HOME/meerora` (or
/// `~/.local/share/meerora`) when it exists, else `dev_fallback`, which the
/// binaries point at [`assets_dir`], the installed assets or the `assets/`
/// folder of the repository.
pub fn resolve_data_dir(dev_fallback: &Path) -> PathBuf {
    if let Some(p) = std::env::var_os("MEERORA_DATA_DIR") {
        return PathBuf::from(p);
    }
    let xdg = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| Path::new(&h).join(".local/share")));
    if let Some(dir) = xdg.map(|d| d.join("meerora"))
        && dir.is_dir()
    {
        return dir;
    }
    dev_fallback.to_path_buf()
}

/// Contents of `agents/<id>/agent.toml` (D21).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentConfig {
    pub name: String,
    #[serde(default = "default_color")]
    pub color: String,
    /// Figure script relative to the agent directory.
    #[serde(default = "default_figure")]
    pub figure: String,
    /// Station where the agent rests between tasks.
    pub home: String,
    /// Standing instructions, added to the system prompt of every run.
    #[serde(default)]
    pub instructions: Option<String>,
    /// What happens when a run you started finishes. Scheduled runs rest
    /// unless the agent asks for you.
    #[serde(default)]
    pub on_done: OnDone,
    /// Windows of the agent view (D11). The chat is always shown.
    #[serde(default = "default_apps")]
    pub apps: Vec<App>,
    /// Folders the agent may read and write besides its own directory, such
    /// as `~/code/project` ([`workspace`]). Their `.git` stays read-only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workspace: Vec<PathBuf>,
    #[serde(default)]
    pub runtime: Option<RuntimeConfig>,
    #[serde(default)]
    pub schedule: Vec<ScheduleEntry>,
    #[serde(default)]
    pub budget: Option<BudgetConfig>,
    #[serde(default)]
    pub capabilities: CapabilitiesConfig,
    /// Set for agents imported from the index (D25). Their runtime is
    /// `external` with the endpoint as `url`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external: Option<ExternalAgent>,
    /// What a special agent is for: `personal` for the user's own agent
    /// (D26), `news` for the news agent (D28), `helper` for the permanent
    /// system agent that configures meerora. Ordinary agents leave it out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<AgentRole>,
}

/// The job of a special agent (D26, D28). It decides which extra meerora
/// tools the agent gets: `draft_post` and `read_feed` for the personal
/// agent, `read_feed` and `post_digest` for the news agent, `config_read`
/// and `config_change` for the helper.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentRole {
    /// The user's own agent, which publishes their profile and drafts posts about them.
    Personal,
    /// Reads the sources the user chose and the feed, and writes a digest every morning.
    News,
    /// The permanent system agent (Rivet) that helps the user configure
    /// meerora and the system. The daemon creates it when no agent has the
    /// role, and it cannot be retired.
    Helper,
}

/// Where an imported external agent comes from (D25).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalAgent {
    /// Manifest id in the index.
    pub manifest: String,
    /// Owner domain, shown on the badge.
    pub domain: String,
    #[serde(default)]
    pub verified: bool,
    /// Base URL of the index it came from.
    pub index: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub privacy: Option<String>,
}

fn default_apps() -> Vec<App> {
    vec![App::Chat, App::Editor, App::Terminal, App::Browser]
}

/// A window of the agent view (D11).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum App {
    Chat,
    Editor,
    Terminal,
    Browser,
    /// The agent's directory: open, hand over and find files.
    Files,
    /// Shows images, Markdown and other results.
    Viewer,
    /// The git history of the agent's directory, one commit per run.
    Changes,
    /// Review, edit, stage, commit and push a repository, such as a
    /// workspace folder (ui/apps/git.rs).
    Git,
    /// Runs, schedule, budget and every action.
    Activity,
}

impl App {
    /// Every app, in the order the launcher lists them.
    pub const ALL: [App; 9] =
        [App::Chat, App::Editor, App::Terminal, App::Browser, App::Files, App::Viewer, App::Changes, App::Git, App::Activity];
}

fn default_color() -> String {
    "#E8EAF0".into()
}

fn default_figure() -> String {
    "figure.js".into()
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnDone {
    /// Come to the front with a full ring (D3).
    #[default]
    Wait,
    Rest,
}

/// Which LLM drives the agent (D22).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeConfig {
    pub kind: RuntimeKind,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RuntimeKind {
    ClaudeCli,
    CodexCli,
    Ollama,
    Api,
    External,
    /// Driven by the scenario of the mock daemon (milestone 1).
    Mock,
}

/// A periodic wake-up (D23).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduleEntry {
    /// Interval such as `"30s"`, `"5m"` or `"1h30m"`. See [`parse_duration`].
    #[serde(default)]
    pub every: Option<String>,
    /// Fixed times in cron syntax. Not used before milestone 2.
    #[serde(default)]
    pub at: Option<String>,
    pub task: String,
    /// Station to walk to for this task.
    #[serde(default)]
    pub station: Option<String>,
    /// Local time range without runs, for example `"22:00-07:00"`.
    #[serde(default)]
    pub quiet_hours: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BudgetConfig {
    #[serde(default)]
    pub tokens_per_day: Option<u64>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilitiesConfig {
    /// Acts with OS privileges and gets the double violet ring (D3, D10).
    #[serde(default)]
    pub system: bool,
    /// Declared capabilities the user approved (D8). Runtimes read `shell`
    /// (run commands in the agent directory) and `web` (fetch and search).
    /// Without them an agent can only read and edit its own directory (D29).
    #[serde(default)]
    pub granted: Vec<String>,
}

impl CapabilitiesConfig {
    pub fn has(&self, capability: &str) -> bool {
        self.granted.iter().any(|g| g == capability)
    }
}

/// Contents of `meerora.toml` at the top of the data dir. Every field has a default.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    /// Runtime that writes figure scripts in the builder (screen 3).
    #[serde(default)]
    pub builder: Option<RuntimeConfig>,
    #[serde(default)]
    pub paths: PathSettings,
    #[serde(default)]
    pub sandbox: SandboxSettings,
    #[serde(default)]
    pub index: IndexSettings,
    #[serde(default)]
    pub desktop: DesktopSettings,
    /// Which room the space is (D6, see [`rooms`]).
    #[serde(default)]
    pub space: SpaceSettings,
    /// `[voice]`, local speech to text and text to speech (D16). The crate
    /// meerora-voice reads and checks it, here it is only accepted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice: Option<toml::Table>,
    /// `[commands]`, the command box on Alt+Space. The app reads and checks
    /// it (ui/commands), here it is only accepted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commands: Option<toml::Table>,
}

/// The agent desktops and the meerora window (D15).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DesktopSettings {
    /// XKB layout of the keymap that apps in agent views get, `us` when unset.
    /// Keys go to them as physical keys, so this should match your keyboard.
    #[serde(default)]
    pub keyboard_layout: Option<String>,
    /// XKB variant, for example `nodeadkeys`.
    #[serde(default)]
    pub keyboard_variant: Option<String>,
    /// While the meerora window has the keyboard, Alt+Tab, Super and other
    /// desktop shortcuts go to meerora (keyboard-shortcuts-inhibit, D15 phase 1).
    #[serde(default = "yes")]
    pub inhibit_shortcuts: bool,
}

impl Default for DesktopSettings {
    fn default() -> Self {
        Self { keyboard_layout: None, keyboard_variant: None, inhibit_shortcuts: true }
    }
}

/// Per-agent sandboxes (D16).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxSettings {
    /// Runs real agents inside bubblewrap. Off runs them like milestone 2 (D29).
    #[serde(default = "yes")]
    pub enabled: bool,
    /// More paths every agent may read, besides the system and its own directory.
    #[serde(default)]
    pub read: Vec<PathBuf>,
}

impl Default for SandboxSettings {
    fn default() -> Self {
        Self { enabled: true, read: Vec::new() }
    }
}

fn yes() -> bool {
    true
}

/// The meerora index (D25, Q10).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IndexSettings {
    /// Base URL, [`DEFAULT_INDEX_URL`] when unset.
    #[serde(default)]
    pub url: Option<String>,
}

/// A local index (`meerora-index serve`) until a public one exists (Q10).
pub const DEFAULT_INDEX_URL: &str = "http://127.0.0.1:7878";

/// Triangle budget of figures from external manifests, below the 3000 of
/// local figures (Q13). The import refuses larger figures.
pub const EXTERNAL_TRIANGLES: usize = 1500;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PathSettings {
    /// `claude` binary, found on PATH when unset.
    #[serde(default)]
    pub claude: Option<PathBuf>,
    #[serde(default)]
    pub codex: Option<PathBuf>,
    /// Ollama base URL, `http://localhost:11434` when unset.
    #[serde(default)]
    pub ollama: Option<String>,
}

impl Settings {
    /// Reads `meerora.toml`. A missing file gives the defaults.
    pub fn load(data_dir: &Path) -> Result<Settings, String> {
        let path = data_dir.join("meerora.toml");
        match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Settings::default()),
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }
}

/// Parses durations like `"45s"`, `"5m"`, `"2h"` and `"1h30m"`.
pub fn parse_duration(s: &str) -> Option<Duration> {
    let mut total = 0u64;
    let mut number = String::new();
    let mut seen_unit = false;
    for c in s.trim().chars() {
        if c.is_ascii_digit() {
            number.push(c);
            continue;
        }
        let n: u64 = number.parse().ok()?;
        number.clear();
        total += n * match c {
            's' => 1,
            'm' => 60,
            'h' => 3600,
            _ => return None,
        };
        seen_unit = true;
    }
    (number.is_empty() && seen_unit && total > 0).then(|| Duration::from_secs(total))
}

/// An agent directory with its parsed config.
#[derive(Clone, Debug, PartialEq)]
pub struct AgentDir {
    pub id: String,
    pub path: PathBuf,
    pub config: AgentConfig,
}

impl AgentDir {
    pub fn figure_path(&self) -> PathBuf {
        self.path.join(&self.config.figure)
    }
}

/// Reads every `agents/<id>/agent.toml` below `data_dir`, sorted by id.
/// Directories without an `agent.toml` are ignored. Broken ones are returned
/// as errors so the caller can show them.
pub fn load_agents(data_dir: &Path) -> Vec<Result<AgentDir, String>> {
    let Ok(entries) = std::fs::read_dir(data_dir.join("agents")) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.join("agent.toml").is_file()).collect();
    dirs.sort();
    dirs.into_iter().map(|path| load_agent(&path)).collect()
}

pub fn load_agent(path: &Path) -> Result<AgentDir, String> {
    let file = path.join("agent.toml");
    let text = std::fs::read_to_string(&file).map_err(|e| format!("{}: {e}", file.display()))?;
    let config: AgentConfig = toml::from_str(&text).map_err(|e| format!("{}: {e}", file.display()))?;
    let id =
        path.file_name().and_then(|n| n.to_str()).ok_or_else(|| format!("{}: directory name is not UTF-8", path.display()))?.to_string();
    Ok(AgentDir { id, path: path.to_path_buf(), config })
}

// Agent view apps (D11, D21).

/// The run history of an agent, relative to its directory, with one
/// [`RunRecord`] per line, oldest first. `logs/` is not committed (D21).
pub const RUNS_LOG: &str = "logs/runs.jsonl";

/// How many runs the daemon keeps in [`RUNS_LOG`].
pub const RUNS_KEPT: usize = 500;

/// One finished run of a real agent, as the daemon appends it to [`RUNS_LOG`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RunRecord {
    pub id: String,
    pub trigger: Trigger,
    pub task: String,
    /// Unix milliseconds.
    pub start: u64,
    /// Unix milliseconds.
    pub end: u64,
    pub outcome: Outcome,
    /// One line for the needs-you card, the answer's first sentence or the error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default)]
    pub tokens: u64,
}

// Graveyard.

/// The folder below the data dir where retired agents wait, one directory each.
pub const GRAVEYARD_DIR: &str = "graveyard";

/// The file the daemon writes into a retired agent's directory.
pub const RETIRED_FILE: &str = "retired.json";

/// Which graveyard command an [`Event::GraveyardResult`] answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GraveyardAction {
    Retire,
    Restore,
    Delete,
}

/// An agent in the graveyard, for the graveyard view and its tombstone.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RetiredAgent {
    /// The name of its directory below [`GRAVEYARD_DIR`]. Restore and
    /// DeleteRetired take it. Usually the old agent id, `<id>-2` and so on
    /// when that name was taken in the graveyard.
    pub id: String,
    /// The id the agent had in the space, which a restore takes again when it is free.
    pub agent: String,
    pub name: String,
    pub color: String,
    /// Unix milliseconds.
    pub retired_at: u64,
    /// The figure script, for the portrait on its card. Empty when it cannot be read.
    #[serde(default)]
    pub figure: String,
    /// The line that stood under its name when it retired.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_activity: Option<String>,
    /// How many runs its history in [`RUNS_LOG`] holds.
    #[serde(default)]
    pub runs: usize,
    /// Its directory, for Open folder and for the message that names it.
    pub path: PathBuf,
}

/// Contents of `graveyard/<id>/retired.json`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RetiredFile {
    /// The agent id in the space.
    pub agent: String,
    pub name: String,
    pub color: String,
    /// Unix milliseconds.
    pub retired_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_activity: Option<String>,
    #[serde(default)]
    pub runs: usize,
}

/// Every directory below `graveyard/` with a [`RETIRED_FILE`] or an
/// `agent.toml`, newest first. Without a readable `retired.json` the name and
/// colour come from `agent.toml` and the time from the directory.
pub fn load_graveyard(data_dir: &Path) -> Vec<RetiredAgent> {
    let Ok(entries) = std::fs::read_dir(data_dir.join(GRAVEYARD_DIR)) else { return Vec::new() };
    let mut agents: Vec<RetiredAgent> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .filter_map(|path| {
            let id = path.file_name()?.to_str()?.to_string();
            let config = load_agent(&path);
            let file = std::fs::read_to_string(path.join(RETIRED_FILE)).ok().and_then(|t| serde_json::from_str::<RetiredFile>(&t).ok());
            let file = match (file, &config) {
                (Some(file), _) => file,
                (None, Ok(dir)) => RetiredFile {
                    agent: id.clone(),
                    name: dir.config.name.clone(),
                    color: dir.config.color.clone(),
                    retired_at: std::fs::metadata(&path)
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map_or(0, |d| d.as_millis() as u64),
                    last_activity: None,
                    runs: 0,
                },
                (None, Err(_)) => return None,
            };
            let figure = config.map(|d| d.figure_path()).unwrap_or_else(|_| path.join(default_figure()));
            Some(RetiredAgent {
                id,
                agent: file.agent,
                name: file.name,
                color: file.color,
                retired_at: file.retired_at,
                figure: std::fs::read_to_string(figure).unwrap_or_default(),
                last_activity: file.last_activity,
                runs: file.runs,
                path,
            })
        })
        .collect();
    agents.sort_by(|a, b| b.retired_at.cmp(&a.retired_at).then_with(|| a.id.cmp(&b.id)));
    agents
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_round_trip_as_tagged_json() {
        let e = Event::Ring { agent: "ember".into(), state: RingState::NeedsYou, progress: None };
        let line = encode(&e);
        assert_eq!(line, "{\"type\":\"ring\",\"agent\":\"ember\",\"state\":\"needs_you\"}\n");
        assert_eq!(decode_event(&line).unwrap(), e);

        let c = Command::Inject { event: Event::GoTo { agent: "wren".into(), station: "door".into() } };
        assert_eq!(decode_command(&encode(&c)).unwrap(), c);

        let p = decode_event(r#"{"type":"param","agent":"wren","name":"letter","value":true}"#).unwrap();
        assert_eq!(p, Event::Param { agent: "wren".into(), name: "letter".into(), value: serde_json::Value::Bool(true) });
    }

    #[test]
    fn action_targets_and_run_starts_are_additive() {
        // Lines of an older daemon still read, without a target or a start.
        let old = decode_event(r#"{"type":"action","agent":"wren","tool":"browser.open","summary":"https://a.example"}"#).unwrap();
        let short = Event::Action { agent: "wren".into(), tool: "browser.open".into(), summary: "https://a.example".into(), target: None };
        assert_eq!(old, short);
        assert!(!encode(&short).contains("target"), "no target, no field");
        let full = Event::Action {
            agent: "wren".into(),
            tool: "browser.open".into(),
            summary: "https://a.example/very/long…".into(),
            target: Some("https://a.example/very/long/path?q=1".into()),
        };
        assert_eq!(decode_event(&encode(&full)).unwrap(), full);

        let old = decode_event(r#"{"type":"run_started","agent":"wren","run":"r1","trigger":"user","task":"Hi"}"#).unwrap();
        assert!(matches!(old, Event::RunStarted { started: 0, .. }), "{old:?}");
        let started = Event::RunStarted {
            agent: "wren".into(),
            run: "r1".into(),
            trigger: Trigger::User,
            task: "Hi".into(),
            started: 1_791_991_823_000,
        };
        assert_eq!(decode_event(&encode(&started)).unwrap(), started);
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("45s"), Some(Duration::from_secs(45)));
        assert_eq!(parse_duration("5m"), Some(Duration::from_secs(300)));
        assert_eq!(parse_duration("1h30m"), Some(Duration::from_secs(5400)));
        assert_eq!(parse_duration("5"), None);
        assert_eq!(parse_duration("0s"), None);
        assert_eq!(parse_duration("5x"), None);
    }

    #[test]
    fn agent_toml_from_the_brief() {
        let text = r##"
name = "Wren"
color = "#D9A441"
figure = "figure.js"
home = "window"

[runtime]
kind = "claude-cli"
model = "opus"

[[schedule]]
every = "5m"
task = "Check the inbox."
station = "mailbox"
quiet_hours = "22:00-07:00"

[budget]
tokens_per_day = 200000

[capabilities]
granted = ["mail.read"]
"##;
        let c: AgentConfig = toml::from_str(text).unwrap();
        assert_eq!(c.runtime.unwrap().kind, RuntimeKind::ClaudeCli);
        assert_eq!(c.schedule[0].station.as_deref(), Some("mailbox"));
        assert!(!c.capabilities.system);
        assert!(toml::from_str::<AgentConfig>("name = \"x\"\nhome = \"a\"\ncolour = \"#fff\"").is_err());
        assert_eq!(c.role, None);
        let personal: AgentConfig = toml::from_str("name = \"Me\"\nhome = \"desk\"\nrole = \"personal\"").unwrap();
        assert_eq!(personal.role, Some(AgentRole::Personal));
    }

    /// The personal profile of section 5 of the brief.
    const PROFILE: &str = r#"{
      "meerora_profile": "0.1",
      "handle": "example",
      "name": { "value": "Alex Example", "visibility": "public" },
      "photos": [{ "url": "https://index.meerora.example/u/example/1.jpg", "visibility": "public" }],
      "headline": { "value": "Writes software, sails on weekends", "visibility": "public" },
      "education": [
        { "school": "Example Gymnasium", "from": 1998, "to": 2007, "visibility": "friends" },
        { "school": "Example University", "degree": "MSc Computer Science", "from": 2007, "to": 2013, "visibility": "public" }
      ],
      "work": [{ "org": "Example GmbH", "role": "Founder", "from": 2020, "visibility": "public" }],
      "places": [{ "value": "Example City", "visibility": "friends" }],
      "interests": [{ "value": "Sailing", "visibility": "public" }],
      "links": [{ "url": "https://example.org", "visibility": "private" }]
    }"#;

    #[test]
    fn the_profile_from_the_brief_parses_and_filters() {
        let p: Profile = serde_json::from_str(PROFILE).unwrap();
        assert!(p.problems().is_empty(), "{:?}", p.problems());
        assert!(!p.is_empty());
        let public = p.visible(Visibility::Public);
        assert_eq!(public.education.len(), 1);
        assert!(public.places.is_empty() && public.links.is_empty());
        let friends = p.visible(Visibility::Friends);
        assert_eq!((friends.education.len(), friends.places.len(), friends.links.len()), (2, 1, 0), "private stays private");
        assert_eq!(p.visible(Visibility::Private), p);
        assert!(serde_json::from_str::<Profile>(r#"{"bio": "x"}"#).is_err(), "unknown fields are refused");
        assert!(serde_json::from_str::<Profile>(r#"{"name": {"value": "A"}}"#).is_err(), "every field needs a visibility");
    }

    #[test]
    fn profile_problems() {
        let mut p: Profile = serde_json::from_str(PROFILE).unwrap();
        p.education[0].from = Some(2010);
        p.work[0].from = Some(1800);
        p.links[0].url = "example.org".into();
        p.headline = Some(ProfileText { value: "two\nlines".into(), visibility: Visibility::Public });
        p.interests = (0..30).map(|i| ProfileText { value: format!("i{i}"), visibility: Visibility::Public }).collect();
        let problems = p.problems().join("\n");
        for needle in ["runs backwards", "1800", "https://", "one line", "interests has 30 entries"] {
            assert!(problems.contains(needle), "{needle} missing in {problems}");
        }
    }

    #[test]
    fn device_messages_round_trip_and_files_are_found() {
        let e = Event::DeviceParam { device: "airplane".into(), name: "flying".into(), value: serde_json::Value::Bool(true) };
        assert_eq!(encode(&e), "{\"type\":\"device_param\",\"device\":\"airplane\",\"name\":\"flying\",\"value\":true}\n");
        assert_eq!(decode_event(&encode(&e)).unwrap(), e);
        assert_eq!(e.agent(), None);
        let info = DeviceInfo {
            id: "airplane".into(),
            name: "Airplane mode".into(),
            owner: None,
            script: "space/devices/airplane.js".into(),
            revision: "1f2e".into(),
            actions: vec![DeviceActionInfo {
                id: "toggle".into(),
                label: "Toggle".into(),
                capability: Some("network.airplane_mode.write".into()),
                opens: None,
            }],
            needs: vec!["network.airplane_mode.write".into()],
            granted: Vec::new(),
            error: None,
        };
        let devices = Event::Devices { devices: vec![info] };
        assert_eq!(decode_event(&encode(&devices)).unwrap(), devices);
        let c = Command::DeviceAction { device: "airplane".into(), action: "toggle".into() };
        assert_eq!(encode(&c), "{\"type\":\"device_action\",\"device\":\"airplane\",\"action\":\"toggle\"}\n");
        assert_eq!(decode_command(&encode(&c)).unwrap(), c);

        let dir = std::env::temp_dir().join(format!("meerora-protocol-devices-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for (path, text) in [
            ("space/devices/tree.js", ""),
            ("space/devices/grants.toml", ""),
            ("agents/rivet/agent.toml", "name = \"Rivet\"\nhome = \"desk\""),
            ("agents/rivet/devices/plane.js", ""),
            ("agents/ghost/devices/lamp.js", ""),
        ] {
            let path = dir.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        let found: Vec<(String, Option<String>)> = device_files(&dir).into_iter().map(|f| (f.id, f.owner)).collect();
        assert_eq!(
            found,
            [("rivet/plane".to_string(), Some("rivet".to_string())), ("tree".to_string(), None)],
            "no agent.toml, no devices"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn social_messages_round_trip() {
        let c = Command::Social { request: "r".into(), action: SocialAction::Befriend { handle: "alex".into() } };
        assert_eq!(encode(&c), "{\"type\":\"social\",\"request\":\"r\",\"action\":{\"action\":\"befriend\",\"handle\":\"alex\"}}\n");
        assert_eq!(decode_command(&encode(&c)).unwrap(), c);
        let t = ToolCall::DraftPost { text: "Hi".into(), visibility: Visibility::Friends };
        assert_eq!(serde_json::to_string(&t).unwrap(), r#"{"tool":"draft_post","text":"Hi","visibility":"friends"}"#);
        let e = Event::PostDrafts { drafts: Vec::new(), published: None };
        assert_eq!(decode_event(&encode(&e)).unwrap(), e);
        assert_eq!(e.agent(), None);
    }

    #[test]
    fn graveyard_messages_round_trip_and_the_graveyard_loads() {
        let c = Command::Retire { agent: "wren".into() };
        assert_eq!(encode(&c), "{\"type\":\"retire\",\"agent\":\"wren\"}\n");
        assert_eq!(decode_command(&encode(&c)).unwrap(), c);
        let r = Event::GraveyardResult { action: GraveyardAction::Delete, agent: "wren-2".into(), id: None, error: Some("no".into()) };
        assert_eq!(encode(&r), "{\"type\":\"graveyard_result\",\"action\":\"delete\",\"agent\":\"wren-2\",\"error\":\"no\"}\n");
        assert_eq!(decode_event(&encode(&r)).unwrap(), r);
        assert_eq!(r.agent(), None, "a graveyard id is not an agent in the space");

        let dir = std::env::temp_dir().join(format!("meerora-protocol-graveyard-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let retired = |at: u64| {
            let file = RetiredFile {
                agent: "wren".into(),
                name: "Wren".into(),
                color: "#D9A441".into(),
                retired_at: at,
                last_activity: Some("checking mail".into()),
                runs: 3,
            };
            serde_json::to_string(&file).unwrap()
        };
        for (path, text) in [
            ("graveyard/wren/retired.json", retired(10)),
            ("graveyard/wren/agent.toml", "name = \"Wren\"\nhome = \"window\"\nfigure = \"bird.js\"".into()),
            ("graveyard/wren/bird.js", "// the bird".into()),
            ("graveyard/wren-2/retired.json", retired(20)),
            ("graveyard/stray/notes.md", String::new()),
            ("graveyard/lost/agent.toml", "name = \"Lost\"\nhome = \"door\"\ncolor = \"#7FE3D0\"".into()),
        ] {
            let path = dir.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        let found = load_graveyard(&dir);
        let ids: Vec<&str> = found.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, ["lost", "wren-2", "wren"], "newest first, and only directories with retired.json or agent.toml");
        assert_eq!((found[0].agent.as_str(), found[0].name.as_str(), found[0].color.as_str()), ("lost", "Lost", "#7FE3D0"));
        assert!(found[0].retired_at > 20, "the time of the directory");
        let found = found[1..].to_vec();
        assert_eq!(found[1].figure, "// the bird", "the figure of agent.toml");
        assert_eq!(found[0].figure, "", "no figure to read");
        assert_eq!((found[0].agent.as_str(), found[0].runs), ("wren", 3));
        assert_eq!(found[1].path, dir.join("graveyard/wren"));
        let e = Event::Graveyard { agents: found };
        assert_eq!(decode_event(&encode(&e)).unwrap(), e);
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(load_graveyard(&dir).is_empty());
    }

    #[test]
    fn desktop_settings_default_to_inhibiting_shortcuts() {
        let s: Settings = toml::from_str("").unwrap();
        assert!(s.desktop.inhibit_shortcuts);
        assert_eq!(s.desktop.keyboard_layout, None);
        let s: Settings = toml::from_str("[desktop]\nkeyboard_layout = \"de\"\ninhibit_shortcuts = false\n").unwrap();
        assert_eq!(s.desktop.keyboard_layout.as_deref(), Some("de"));
        assert!(!s.desktop.inhibit_shortcuts);
    }

    #[test]
    fn voice_settings_are_accepted() {
        let s: Settings = toml::from_str("[voice]\nenabled = false\n[voice.agents.wren]\nspeak_replies = true\n").unwrap();
        assert_eq!(s.voice.and_then(|v| v.get("enabled").cloned()), Some(toml::Value::Boolean(false)));
    }

    #[test]
    fn app_messages_round_trip_and_name_their_capability() {
        let act = ToolCall::AppAct { app: "terminal:1".into(), action: "run".into(), args: serde_json::json!({ "command": "ls" }) };
        assert_eq!(serde_json::to_string(&act).unwrap(), r#"{"tool":"app_act","app":"terminal:1","action":"run","args":{"command":"ls"}}"#);
        let list: ToolCall = serde_json::from_str(r#"{"tool":"apps_list"}"#).unwrap();
        assert_eq!(list, ToolCall::AppsList);
        let open: ToolCall = serde_json::from_str(r#"{"tool":"app_open","app":"terminal"}"#).unwrap();
        assert_eq!(open, ToolCall::AppOpen { app: "terminal".into(), args: serde_json::Value::Null });

        let call = AppCall::from_tool(&act).unwrap();
        assert_eq!((call.tool(), call.app(), call.capability()), ("app_act", Some("terminal:1"), "apps.control"));
        let cases = [
            (ToolCall::AppsList, "apps.read"),
            (ToolCall::AppRead { app: "browser:1".into(), query: None }, "apps.read"),
            (ToolCall::AppScreenshot { app: "window:7".into() }, "apps.read"),
            (ToolCall::AppAct { app: "window:7".into(), action: "type".into(), args: serde_json::Value::Null }, "apps.input"),
            (ToolCall::AppOpen { app: "browser".into(), args: serde_json::Value::Null }, "apps.control"),
            (ToolCall::AppOpen { app: "org.gnome.Calculator.desktop".into(), args: serde_json::Value::Null }, "apps.launch"),
        ];
        for (tool, capability) in cases {
            assert_eq!(AppCall::from_tool(&tool).unwrap().capability(), capability, "{tool:?}");
        }
        assert_eq!(AppCall::from_tool(&ToolCall::Note { text: "hi".into() }), None);

        let e = Event::AppRequest { id: "app-1".into(), agent: "wren".into(), request: call, granted: vec!["apps.control".into()] };
        assert_eq!(decode_event(&encode(&e)).unwrap(), e);
        assert_eq!(e.agent(), Some("wren"));
        let mut result = AppResult::text("done");
        result.log = Some(AppLog { tool: "terminal.run".into(), summary: "ls".into() });
        let c = Command::AppReply { id: "app-1".into(), result };
        assert_eq!(decode_command(&encode(&c)).unwrap(), c);
        assert_eq!(encode(&Command::ServeApps), "{\"type\":\"serve_apps\"}\n");

        let plain = Event::ToolResult { id: "t".into(), ok: true, message: "m".into(), image: None };
        assert_eq!(encode(&plain), "{\"type\":\"tool_result\",\"id\":\"t\",\"ok\":true,\"message\":\"m\"}\n", "older bridges read it");
        let image = Event::ToolResult { id: "t".into(), ok: true, message: "m".into(), image: Some("iVBO".into()) };
        assert_eq!(decode_event(&encode(&image)).unwrap(), image);
    }
}
