//! The agent and app standard in the app (D30, docs/design/agent-apps.md).
//!
//! The daemon checks an agent's app tool call against its capabilities and
//! hands it to the app as `Event::AppRequest`, because the app holds the
//! terminals, browsers, editors and windows. This module answers it with
//! `Command::AppReply`.
//!
//! Every window of an agent's view is an app instance with a handle
//! (`AppTab::handle`). [`app_for`] gives the [`AgentApp`] of a handle, and
//! each kind implements that trait: reading itself as text, performing the
//! actions of its vocabulary and, if it has pixels, a screenshot. A new app
//! joins the standard by implementing the trait and adding itself to
//! [`app_for`]. Nothing else has to change.
//!
//! Work that takes a while, such as a browser page that reads itself on
//! Chrome's side or a terminal command whose output has to settle, answers
//! with [`Reply::Later`], which [`step`] polls once per frame. The app also
//! serves requests between frames (`Core::serve_apps`), so an agent can work
//! while the meerora window is hidden.
//!
//! Visible control (D12): a successful act returns a line for the agent's
//! action log, `<kind>.<action> <summary>`, which the daemon sends to every
//! client. The tile head says what the agent does for a few seconds
//! ([`doing`]), and Take over makes every act on that instance fail with
//! [`TAKEN_OVER`].

mod browser;
mod files;
mod git;
mod keys;
mod open;
mod summaries;
mod terminal;
#[cfg(test)]
mod tests;
mod window;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use base64::Engine as _;
use meerora_protocol::{AppCall, AppLog, AppResult, Command, Event};
use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::engine::Core;
use crate::ui::{AppTab, View};

/// The most characters of `content` that `app_read` returns.
pub const CONTENT_MAX: usize = 8000;
/// The most elements that `app_read` returns.
pub const ELEMENTS_MAX: usize = 150;
/// Seconds a tile head says "<Agent> is typing" after an act.
pub const ACTIVE_FOR: f64 = 3.0;
/// Seconds an instance counts as controlled by the agent after an act.
const CONTROL_FOR: f64 = 30.0;
/// How long work that answers later may take. The daemon waits 30 s.
const LATER_MAX: Duration = Duration::from_secs(25);
/// The error of every act on an instance the user took over (D12).
pub const TAKEN_OVER: &str = "the user took over this app. Wait until they hand it back, or ask them with ask_user.";

/// What an app instance shows, as text (D30).
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct AppText {
    pub handle: String,
    pub kind: String,
    pub title: String,
    /// One line about it.
    pub summary: String,
    /// A few kind-specific fields such as `url` or `cwd`.
    #[serde(skip_serializing_if = "Map::is_empty")]
    pub extra: Map<String, Value>,
    /// What the agent can act on.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub elements: Vec<Element>,
    /// The visible text, cut to [`CONTENT_MAX`] with a marker.
    pub content: String,
    /// Cut the content at its start rather than its end, for the terminal, whose newest lines are last.
    #[serde(skip)]
    pub keep_end: bool,
}

/// Something an agent can act on, with the `ref` it passes to `app_act`.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Element {
    #[serde(rename = "ref")]
    pub r#ref: String,
    pub role: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
}

/// What a tile head says the agent does (D12).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verb {
    Typing,
    Clicking,
    Scrolling,
    Opening,
}

impl Verb {
    pub fn word(self) -> &'static str {
        match self {
            Verb::Typing => "typing",
            Verb::Clicking => "clicking",
            Verb::Scrolling => "scrolling",
            Verb::Opening => "opening",
        }
    }

    fn of(action: &str) -> Verb {
        match action {
            "type" | "key" | "run" | "commit" => Verb::Typing,
            "click" | "stage" | "unstage" => Verb::Clicking,
            "scroll" => Verb::Scrolling,
            _ => Verb::Opening,
        }
    }
}

/// What an act did: a sentence for the model and the summary for the
/// action log, such as the command of `terminal.run`.
#[derive(Clone, Debug, PartialEq)]
pub struct Acted {
    pub text: String,
    pub summary: String,
}

impl Acted {
    pub fn new(text: impl Into<String>, summary: impl Into<String>) -> Self {
        Self { text: text.into(), summary: summary.into() }
    }
}

/// Work that polls until it has its answer, once per frame.
pub type Poll<T> = Box<dyn FnMut(&mut Core) -> Option<Result<T, String>>>;

/// The answer of an app, now or after a while.
pub enum Reply<T> {
    Now(Result<T, String>),
    Later(Poll<T>),
}

impl<T: 'static> Reply<T> {
    pub fn ok(value: T) -> Self {
        Reply::Now(Ok(value))
    }

    pub fn err(text: impl Into<String>) -> Self {
        Reply::Now(Err(text.into()))
    }

    /// Runs `work` on a thread and answers when it is done, for blocking
    /// work such as CDP calls to Chrome.
    pub fn thread(work: impl FnOnce() -> Result<T, String> + Send + 'static) -> Self
    where
        T: Send,
    {
        let (tx, rx) = std::sync::mpsc::channel();
        let spawned = std::thread::Builder::new().name("agent-app".into()).spawn(move || {
            let _ = tx.send(work());
        });
        if let Err(e) = spawned {
            return Reply::err(format!("meerora could not start the work ({e})."));
        }
        Reply::Later(Box::new(move |_| match rx.try_recv() {
            Ok(result) => Some(result),
            Err(std::sync::mpsc::TryRecvError::Empty) => None,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => Some(Err("the work stopped before it answered".into())),
        }))
    }

    /// Turns the answer into an [`AppResult`] once it is there, with `core` at hand.
    fn finish(self, core: &mut Core, done: impl FnOnce(&mut Core, Result<T, String>) -> AppResult + 'static) -> Reply<AppResult> {
        match self {
            Reply::Now(result) => Reply::Now(Ok(done(core, result))),
            Reply::Later(mut poll) => {
                let mut done = Some(done);
                Reply::Later(Box::new(move |core| {
                    let result = poll(core)?;
                    let done = done.take()?;
                    Some(Ok(done(core, result)))
                }))
            }
        }
    }
}

/// One kind of app as agents use it (D30). Implementations are small
/// handles (agent and instance) that find their state in [`Core`].
pub trait AgentApp {
    /// The kind, as in handles: `terminal`, `browser` and so on.
    fn kind(&self) -> &'static str;
    /// The actions of its vocabulary, besides focus and close of every kind.
    fn actions(&self) -> &'static [&'static str];
    /// What the instance shows as text.
    fn read(&mut self, core: &mut Core) -> Reply<AppText>;
    /// One action of [`AgentApp::actions`] with its arguments.
    fn act(&mut self, core: &mut Core, action: &str, args: &Map<String, Value>) -> Reply<Acted>;
    /// A PNG of the instance as the user sees it. None for kinds without pixels.
    fn screenshot(&mut self, _core: &mut Core) -> Option<Reply<Vec<u8>>> {
        None
    }
    /// The user took the instance over (D12), so acts fail.
    fn taken_over(&self, _core: &mut Core) -> bool {
        false
    }
}

/// The app of a handle of `agent`. A new kind joins the standard here.
#[allow(unreachable_patterns, reason = "kinds that other changes add go through the last arm until they join")]
pub fn app_for(agent: &str, tab: AppTab) -> Option<Box<dyn AgentApp>> {
    let agent = agent.to_string();
    Some(match tab {
        AppTab::Terminal(n) => Box::new(terminal::TerminalApp { agent, n }),
        AppTab::Browser(n) => Box::new(browser::BrowserApp { agent, n }),
        AppTab::Editor(n) => Box::new(files::EditorApp { agent, n }),
        AppTab::Files(n) => Box::new(files::FilesApp { agent, n }),
        AppTab::Viewer(n) => Box::new(files::ViewerApp { agent, n }),
        AppTab::Chat => Box::new(summaries::ChatApp { agent }),
        AppTab::Changes => Box::new(summaries::ChangesApp { agent }),
        AppTab::Activity => Box::new(summaries::ActivityApp { agent }),
        AppTab::Git(n) => Box::new(git::GitApp { agent, n }),
        AppTab::Window(id) => Box::new(window::WindowApp { agent, id }),
        _ => return None,
    })
}

/// The kind of a handle, its part before the colon.
pub fn kind_of(tab: AppTab) -> String {
    let handle = tab.handle();
    handle.split(':').next().unwrap_or(&handle).to_string()
}

/// A request that waits for its answer.
struct Waiting {
    id: String,
    started: Instant,
    poll: Poll<AppResult>,
}

/// The app's side of D30: requests, answers and what the tile heads show.
#[derive(Default)]
pub struct AgentApps {
    /// Requests from the daemon that were not handled yet.
    queue: Vec<(String, String, AppCall, Vec<String>)>,
    waiting: Vec<Waiting>,
    /// Answers for the daemon. [`step`] sends them, tests read them.
    pub outbox: Vec<(String, AppResult)>,
    /// What the agent did last in each instance and when, for the tile heads.
    activity: HashMap<(String, AppTab), (Verb, f64)>,
    /// Instances without a Take over of their own that the user took over, such as windows.
    taken: HashSet<(String, AppTab)>,
    /// Where the agent clicked last in a window, in points, and when, for its cursor.
    clicks: HashMap<(String, u32), (f32, f32, f64)>,
    /// `browser.open` lines of acts, so the browser does not open them a second time when the daemon logs them.
    echoes: Vec<(String, String)>,
    /// Wakes the event loop while work waits, so it is polled when no frames are drawn.
    ticker: Option<Arc<AtomicBool>>,
}

impl AgentApps {
    /// The user took over an instance without a Take over of its own, or handed it back.
    pub fn set_taken(&mut self, agent: &str, tab: AppTab, taken: bool) {
        let key = (agent.to_string(), tab);
        if taken {
            self.taken.insert(key);
            self.activity.remove(&(agent.to_string(), tab));
        } else {
            self.taken.remove(&key);
        }
    }

    pub fn is_taken(&self, agent: &str, tab: AppTab) -> bool {
        self.taken.contains(&(agent.to_string(), tab))
    }

    /// Where and when the agent last clicked in window `id`.
    pub fn last_click(&self, agent: &str, id: u32) -> Option<(f32, f32, f64)> {
        self.clicks.get(&(agent.to_string(), id)).copied()
    }

    /// When the agent last acted on an instance.
    pub fn last_act(&self, agent: &str, tab: AppTab) -> Option<(Verb, f64)> {
        self.activity.get(&(agent.to_string(), tab)).copied()
    }
}

/// What the agent does in an instance right now, for its tile head: "typing",
/// "clicking" and so on, for [`ACTIVE_FOR`] seconds after an act.
pub fn doing(core: &Core, agent: &str, tab: AppTab) -> Option<&'static str> {
    let (verb, at) = core.agent_apps.last_act(agent, tab)?;
    (core.world.time - at < ACTIVE_FOR).then_some(verb.word())
}

/// An app request from the daemon. It is handled with the next [`step`].
pub fn on_request(core: &mut Core, event: &Event) -> bool {
    let Event::AppRequest { id, agent, request, granted } = event else { return false };
    core.agent_apps.queue.push((id.clone(), agent.clone(), request.clone(), granted.clone()));
    true
}

/// Whether a logged action is the echo of a browser page an act opened, and
/// forgets it. The browser then does not open it a second time.
pub fn take_echo(core: &mut Core, event: &Event) -> bool {
    let Event::Action { agent, tool, summary, target } = event else { return false };
    if tool != "browser.open" {
        return false;
    }
    let full = target.as_deref().unwrap_or(summary);
    let echoes = &mut core.agent_apps.echoes;
    match echoes.iter().position(|(a, s)| a == agent && s == full) {
        Some(i) => {
            echoes.remove(i);
            true
        }
        None => false,
    }
}

/// Handles new requests, polls waiting work and sends the answers.
pub fn step(core: &mut Core) {
    for (id, agent, call, granted) in std::mem::take(&mut core.agent_apps.queue) {
        match handle(core, &agent, call, &granted) {
            Reply::Now(result) => {
                let result = result.unwrap_or_else(AppResult::error);
                core.agent_apps.outbox.push((id, result));
            }
            Reply::Later(poll) => core.agent_apps.waiting.push(Waiting { id, started: Instant::now(), poll }),
        }
    }
    let mut still = Vec::new();
    for mut w in std::mem::take(&mut core.agent_apps.waiting) {
        match (w.poll)(core) {
            Some(result) => core.agent_apps.outbox.push((w.id, result.unwrap_or_else(AppResult::error))),
            None if w.started.elapsed() > LATER_MAX => {
                let text = format!("meerora gave up after {} seconds, so it is unclear whether it took effect.", LATER_MAX.as_secs());
                core.agent_apps.outbox.push((w.id, AppResult::error(text)));
            }
            None => still.push(w),
        }
    }
    core.agent_apps.waiting.extend(still);
    if let Some(link) = &core.link {
        for (id, result) in core.agent_apps.outbox.drain(..) {
            link.send(&Command::AppReply { id, result });
        }
    }
    tick_while_waiting(core);
}

/// Keeps the event loop waking every 50 ms while work waits.
fn tick_while_waiting(core: &mut Core) {
    let busy = !core.agent_apps.waiting.is_empty();
    match (&core.agent_apps.ticker, busy) {
        (Some(flag), false) => {
            flag.store(false, Ordering::Relaxed);
            core.agent_apps.ticker = None;
        }
        (None, true) => {
            let flag = Arc::new(AtomicBool::new(true));
            let (run, wake) = (flag.clone(), core.waker());
            let spawned = std::thread::Builder::new().name("agent-apps-tick".into()).spawn(move || {
                while run.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(50));
                    wake();
                }
            });
            if spawned.is_ok() {
                core.agent_apps.ticker = Some(flag);
            }
        }
        _ => {}
    }
}

/// The answer to one request.
fn handle(core: &mut Core, agent: &str, call: AppCall, granted: &[String]) -> Reply<AppResult> {
    if core.world.agent(agent).is_none() {
        return Reply::err(format!("meerora does not know an agent {agent:?}."));
    }
    match call {
        AppCall::List => Reply::ok(list(core, agent)),
        AppCall::Read { app, query } => match instance(core, agent, &app) {
            Ok((tab, mut kind)) => {
                let handle = tab.handle();
                kind.read(core).finish(core, move |_, result| match result {
                    Ok(text) => AppResult::text(render(text, &handle, query.as_deref())),
                    Err(e) => AppResult::error(e),
                })
            }
            Err(e) => Reply::err(e),
        },
        AppCall::Screenshot { app } => match instance(core, agent, &app) {
            Ok((tab, mut kind)) => {
                let handle = tab.handle();
                match kind.screenshot(core) {
                    Some(reply) => reply.finish(core, move |_, result| picture(&handle, result)),
                    None => Reply::err(format!("{handle} has no picture. Read it with app_read, which returns its text.")),
                }
            }
            Err(e) => Reply::err(e),
        },
        AppCall::Act { app, action, args } => {
            let args = match args {
                Value::Object(map) => map,
                Value::Null => Map::new(),
                other => return Reply::err(format!("args must be an object, not {other}.")),
            };
            match instance(core, agent, &app) {
                Ok((tab, kind)) => act(core, agent, tab, kind, &action, &args),
                Err(e) => Reply::err(e),
            }
        }
        AppCall::Open { app, args } => {
            let args = match args {
                Value::Object(map) => map,
                Value::Null => Map::new(),
                other => return Reply::err(format!("args must be an object, not {other}.")),
            };
            let agent_id = agent.to_string();
            open::open(core, agent, &app, &args, granted).finish(core, move |core, result| match result {
                Ok((tab, acted)) => logged(core, &agent_id, tab, "open", acted),
                Err(e) => AppResult::error(e),
            })
        }
    }
}

/// One act on an instance: the common focus and close, or the kind's own action.
fn act(
    core: &mut Core,
    agent: &str,
    tab: AppTab,
    mut kind: Box<dyn AgentApp>,
    action: &str,
    args: &Map<String, Value>,
) -> Reply<AppResult> {
    let handle = tab.handle();
    let name = kind.kind();
    if action != "focus" && kind.taken_over(core) {
        return Reply::err(format!("{handle}: {TAKEN_OVER}"));
    }
    let reply = match action {
        "focus" => Reply::Now(focus(core, agent, tab)),
        "close" => Reply::Now(close(core, agent, tab)),
        _ if kind.actions().contains(&action) => {
            core.agent_apps.activity.insert((agent.to_string(), tab), (Verb::of(action), core.world.time));
            kind.act(core, action, args)
        }
        _ => {
            let actions: Vec<&str> = kind.actions().iter().copied().chain(["focus", "close"]).collect();
            return Reply::err(format!("{handle} has no action {action:?}. A {name} takes: {}.", actions.join(", ")));
        }
    };
    let (agent, action) = (agent.to_string(), action.to_string());
    reply.finish(core, move |core, result| match result {
        Ok(acted) => logged(core, &agent, tab, &action, acted),
        Err(e) => AppResult::error(e),
    })
}

/// The answer to a successful act or open, with its line for the action log.
fn logged(core: &mut Core, agent: &str, tab: AppTab, action: &str, acted: Acted) -> AppResult {
    let kind = kind_of(tab);
    core.agent_apps.activity.insert((agent.to_string(), tab), (Verb::of(action), core.world.time));
    let tool = format!("{kind}.{action}");
    if tool == "browser.open" {
        core.agent_apps.echoes.push((agent.to_string(), acted.summary.clone()));
    }
    let mut result = AppResult::text(acted.text);
    result.log = Some(AppLog { tool, summary: acted.summary });
    result
}

/// The open instances of `agent`, creating its tile layout when its view never showed.
pub fn open_tabs(core: &mut Core, agent: &str) -> Vec<AppTab> {
    let apps = crate::ui::agent::apps_of(core, agent);
    let content = content_rect();
    crate::ui::tiles::layout_for(core, agent, &apps, content);
    crate::ui::agent::open_tabs(core, agent)
}

/// Where the tiles go on a usual screen, for layouts made without a view.
pub fn content_rect() -> egui::Rect {
    crate::ui::agent::content_rect(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1440.0, 900.0)))
}

/// The tab and app of `handle`, when it is open in the agent's view.
fn instance(core: &mut Core, agent: &str, handle: &str) -> Result<(AppTab, Box<dyn AgentApp>), String> {
    let Some(tab) = AppTab::parse_handle(&handle.trim().to_lowercase()) else {
        return Err(format!("{handle:?} is not a handle. Handles look like terminal:1, browser:2, chat or window:7, see apps_list."));
    };
    let open = open_tabs(core, agent);
    let belongs = match tab {
        AppTab::Window(id) => core.desktops.window(id).is_some_and(|w| w.desktop == agent),
        _ => open.contains(&tab),
    };
    if !belongs {
        let handles: Vec<String> = open.iter().map(|t| t.handle()).collect();
        return Err(format!("There is no {} open in your view. Open are: {}. app_open opens a new one.", tab.handle(), handles.join(", ")));
    }
    let app = app_for(agent, tab).ok_or_else(|| format!("{} does not take part in the app tools yet.", tab.handle()))?;
    Ok((tab, app))
}

/// `apps_list`: every open instance with its kind, title, focus and control.
fn list(core: &mut Core, agent: &str) -> AppResult {
    let now = core.world.time;
    let viewing = core.ui.view == View::Agent(agent.to_string());
    let mut apps = Vec::new();
    for tab in open_tabs(core, agent) {
        let taken = app_for(agent, tab).is_some_and(|kind| kind.taken_over(core));
        let recent = core.agent_apps.last_act(agent, tab).is_some_and(|(_, at)| now - at < CONTROL_FOR);
        let control = if taken {
            "user"
        } else if recent {
            "you"
        } else {
            "nobody"
        };
        let mut entry = json!({
            "handle": tab.handle(),
            "kind": kind_of(tab),
            "title": crate::ui::tab_title(core, tab),
            "focused": viewing && core.ui.tab == tab,
            "control": control,
        });
        if let AppTab::Window(id) = tab
            && let Some(w) = core.desktops.window(id)
        {
            entry["sandboxed"] = json!(w.sandboxed);
            entry["app_id"] = json!(w.app_id);
        }
        apps.push(entry);
    }
    let text = json!({
        "apps": apps,
        "user_watches_your_view": viewing,
        "control": "you means you acted on it in the last 30 seconds, user means the user took it over and your actions on it fail",
        "open_more_with_app_open": ["terminal", "browser", "editor", "files", "viewer"],
    });
    AppResult::text(text.to_string())
}

/// `focus`: makes the instance the selected tab of the agent's view.
fn focus(core: &mut Core, agent: &str, tab: AppTab) -> Result<Acted, String> {
    if let Some(app) = tab.app().filter(|a| crate::ui::multi(*a)) {
        core.ui.last_instance.insert((agent.to_string(), app), tab.instance());
    }
    let handle = tab.handle();
    if core.ui.view == View::Agent(agent.to_string()) {
        core.ui.tab = tab;
        Ok(Acted::new(format!("{handle} is the selected tab of your view now."), handle))
    } else {
        Ok(Acted::new(format!("{handle} is selected when the user opens your view. They look at something else now."), handle))
    }
}

/// `close`: closes the instance like the cross on its tile head.
fn close(core: &mut Core, agent: &str, tab: AppTab) -> Result<Acted, String> {
    let handle = tab.handle();
    match tab {
        AppTab::Chat => return Err("The chat cannot be closed.".into()),
        AppTab::Window(id) => {
            crate::ui::window::request_close(core, id);
            return Ok(Acted::new(format!("Asked {handle} to close. Its tile goes once the window is gone."), handle));
        }
        _ => {}
    }
    let apps = crate::ui::agent::apps_of(core, agent);
    let content = content_rect();
    if !crate::ui::tiles::layout_for(core, agent, &apps, content).close(tab) {
        return Err(format!("{handle} is not open."));
    }
    match tab {
        AppTab::Terminal(n) => core.close_terminal(agent, n),
        AppTab::Browser(n) => core.browsers.close_instance(agent, n),
        _ => {}
    }
    if core.ui.view == View::Agent(agent.to_string()) && core.ui.tab == tab {
        core.ui.tab = AppTab::Chat;
    }
    crate::ui::tiles::save_layouts(core);
    Ok(Acted::new(format!("Closed {handle}."), handle))
}

/// Opens `tab` in the agent's layout beside its siblings, without taking
/// the user's selected tab away.
pub fn open_tile(core: &mut Core, agent: &str, tab: AppTab) {
    let apps = crate::ui::agent::apps_of(core, agent);
    let content = content_rect();
    let layout = crate::ui::tiles::layout_for(core, agent, &apps, content);
    let open = layout.apps();
    let near = open.iter().copied().filter(|t| *t != tab && t.app().is_some() && t.app() == tab.app()).max_by_key(|t| t.instance());
    match near {
        Some(near) => layout.open_beside(tab, near, content),
        None => layout.open(tab, content),
    }
    crate::ui::tiles::save_layouts(core);
}

/// The `app_read` answer: the text filtered by `query`, cut and as JSON.
fn render(mut text: AppText, handle: &str, query: Option<&str>) -> String {
    text.handle = handle.to_string();
    if let Some(query) = query.map(str::trim).filter(|q| !q.is_empty()) {
        let words: Vec<String> = query.to_lowercase().split_whitespace().map(String::from).collect();
        let hit = |s: &str| {
            let s = s.to_lowercase();
            words.iter().all(|w| s.contains(w.as_str()))
        };
        let lines: Vec<&str> = text.content.lines().filter(|l| hit(l)).collect();
        text.content = if lines.is_empty() { format!("[No line contains {query:?}.]") } else { lines.join("\n") };
        text.elements.retain(|e| hit(&format!("{} {} {}", e.role, e.name, e.value.as_deref().unwrap_or(""))));
        text.extra.insert("query".into(), json!(query));
    }
    let total = text.content.chars().count();
    if total > CONTENT_MAX {
        let cut = total - CONTENT_MAX;
        let marker = format!("[{cut} more characters cut. Pass a query to app_read to find a part.]");
        text.content = if text.keep_end {
            let rest: String = text.content.chars().skip(cut).collect();
            format!("{marker}\n{rest}")
        } else {
            let rest: String = text.content.chars().take(CONTENT_MAX).collect();
            format!("{rest}\n{marker}")
        };
    }
    if text.elements.len() > ELEMENTS_MAX {
        let more = text.elements.len() - ELEMENTS_MAX;
        text.elements.truncate(ELEMENTS_MAX);
        text.extra.insert("elements_cut".into(), json!(format!("{more} more elements cut. Pass a query to find one.")));
    }
    serde_json::to_string(&text).unwrap_or_else(|e| format!("{{\"error\": \"{e}\"}}"))
}

/// The `app_screenshot` answer: a sentence and the PNG as base64.
fn picture(handle: &str, result: Result<Vec<u8>, String>) -> AppResult {
    match result {
        Ok(png) => {
            let (w, h) = png_size(&png).unwrap_or((0, 0));
            let mut result = AppResult::text(format!("A picture of {handle}, {w} by {h} pixels."));
            result.image = Some(base64::engine::general_purpose::STANDARD.encode(&png));
            result
        }
        Err(e) => AppResult::error(e),
    }
}

/// Width and height from a PNG's header.
fn png_size(png: &[u8]) -> Option<(u32, u32)> {
    if png.len() < 24 || &png[..8] != b"\x89PNG\r\n\x1a\n" {
        return None;
    }
    let word = |at: usize| u32::from_be_bytes([png[at], png[at + 1], png[at + 2], png[at + 3]]);
    Some((word(16), word(20)))
}

/// Encodes an RGBA image as PNG, at most `max_width` pixels wide.
pub fn encode_png(image: image::RgbaImage, max_width: u32) -> Result<Vec<u8>, String> {
    let image = if image.width() > max_width {
        let height = (image.height() as f32 * max_width as f32 / image.width() as f32).round().max(1.0) as u32;
        image::imageops::resize(&image, max_width, height, image::imageops::FilterType::Triangle)
    } else {
        image
    };
    let mut png = Vec::new();
    image::DynamicImage::ImageRgba8(image)
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .map_err(|e| format!("the picture could not be encoded ({e})"))?;
    Ok(png)
}

// Arguments of actions.

/// A string argument that must be there.
pub fn text_arg(args: &Map<String, Value>, key: &str, action: &str) -> Result<String, String> {
    match args.get(key) {
        Some(Value::String(s)) => Ok(s.clone()),
        Some(Value::Number(n)) => Ok(n.to_string()),
        Some(other) => Err(format!("{key} of {action} must be a string, not {other}.")),
        None => Err(format!("{action} needs {key} in args, for example {{\"{key}\": ...}}.")),
    }
}

/// A number argument, `default` when it is missing.
pub fn number_arg(args: &Map<String, Value>, key: &str, action: &str, default: Option<f64>) -> Result<f64, String> {
    match args.get(key) {
        Some(Value::Number(n)) => n.as_f64().filter(|v| v.is_finite()).ok_or_else(|| format!("{key} of {action} is not a number.")),
        Some(Value::String(s)) => s.trim().parse::<f64>().map_err(|_| format!("{key} of {action} must be a number, not {s:?}.")),
        None | Some(Value::Null) => default.ok_or_else(|| format!("{action} needs {key} in args.")),
        Some(other) => Err(format!("{key} of {action} must be a number, not {other}.")),
    }
}

pub fn bool_arg(args: &Map<String, Value>, key: &str) -> bool {
    match args.get(key) {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => s == "true" || s == "yes",
        _ => false,
    }
}

/// `path` of `open {path}` inside the agent's directory `dir`, relative to
/// it. The path is resolved with every symbolic link, so a link that points
/// out of the directory is refused like `../` or a foreign absolute path.
pub fn inside(dir: &Path, path: &str) -> Result<PathBuf, String> {
    let trimmed = path.trim();
    let outside = || format!("{trimmed} is outside your directory. Paths are relative to your directory {}.", dir.display());
    if trimmed.is_empty() {
        return Err("The path is empty.".into());
    }
    let root = dir.canonicalize().map_err(|e| format!("Your directory {} cannot be read ({e}).", dir.display()))?;
    let raw = if Path::new(trimmed).is_absolute() { PathBuf::from(trimmed) } else { root.join(trimmed) };
    let real = raw.canonicalize().map_err(|_| format!("{trimmed} does not exist in your directory."))?;
    let rel = real.strip_prefix(&root).map_err(|_| outside())?;
    Ok(rel.to_path_buf())
}
