//! What the command box shows for a query. Every provider turns the query
//! into rows with a score: apps, agents, math, the LLM, the web, meerora's
//! own places, the user's keywords and scripts. The rows only describe what
//! Enter and Shift+Enter would do ([`Act`]). `mod.rs` carries that out, so
//! this file needs no `Core` and is tested on plain data.

use std::collections::HashMap;

use meerora_protocol::App;
use meerora_script::{CommandAction, CommandItem, CommandMeta};

use super::calc;
use super::config::{BoxSettings, Entry, encode};
use super::fuzzy;
use crate::desktop_apps::DesktopApp;
use crate::ui::AppTab;

/// The most rows the box keeps for one query.
pub const MAX_ROWS: usize = 40;

/// An agent as the box sees it.
#[derive(Clone, Debug, PartialEq)]
pub struct AgentRef {
    pub id: String,
    pub name: String,
    pub color: [u8; 3],
    /// Played by the demo scenario, so messages get only a note back.
    pub mock: bool,
}

/// A script as the box sees it.
pub struct ScriptView<'a> {
    pub name: &'a str,
    pub meta: Option<&'a CommandMeta>,
    pub error: Option<&'a str>,
    /// The newest answer: its query and the items or the error.
    pub answer: Option<&'a (String, Result<Vec<CommandItem>, String>)>,
}

/// The one-shot answer of `??`.
#[derive(Clone, Debug, PartialEq)]
pub struct Answer {
    pub request: String,
    pub agent: String,
    pub question: String,
    pub text: String,
    pub done: bool,
    pub error: Option<String>,
}

/// Everything the providers read.
pub struct Input<'a> {
    pub query: &'a str,
    pub agents: &'a [AgentRef],
    /// Where apps start and pages open.
    pub target: Option<&'a AgentRef>,
    pub settings: &'a BoxSettings,
    pub entries: &'a [Entry],
    /// Problems with `meerora.toml` and `commands.toml`.
    pub errors: &'a [String],
    pub apps: &'a [DesktopApp],
    /// `recent-apps.json` of the launcher, desktop file id to unix seconds.
    pub app_recent: &'a HashMap<String, u64>,
    /// What the box ran, row id to unix seconds.
    pub history: &'a HashMap<String, u64>,
    pub scripts: Vec<ScriptView<'a>>,
    pub answer: Option<&'a Answer>,
    pub daemon: bool,
    /// Unix seconds.
    pub now: u64,
}

/// The icon square of a row.
#[derive(Clone, Debug, PartialEq)]
pub enum Glyph {
    /// An installed app, drawn with the first letter of its name.
    App(String),
    Builtin(App),
    /// The agent's portrait.
    Agent(String),
    Math,
    Ask,
    Web,
    Place,
    /// A keyword of `commands.toml`, with its first letters.
    Command(String),
    /// A script item, with the item's glyph.
    Script(String),
    Error,
}

/// Where a place row goes.
#[derive(Clone, Debug, PartialEq)]
pub enum Place {
    Space,
    Feed,
    Graveyard,
    Rooms,
    /// A room by id, with its name.
    Room(String, String),
    NewAgent,
    Index,
}

/// What Enter or Shift+Enter does.
#[derive(Clone, Debug, PartialEq)]
pub enum Act {
    /// Starts an installed app on `agent`'s desktop. `show` opens its view.
    Launch {
        app: DesktopApp,
        agent: String,
        show: bool,
    },
    /// Starts a program with arguments on `agent`'s desktop.
    Run {
        agent: String,
        program: String,
        args: Vec<String>,
        show: bool,
    },
    /// Opens a built-in app in `agent`'s view, a new instance with `new`.
    Builtin {
        agent: String,
        app: App,
        new: bool,
    },
    /// Opens an app instance by handle in `agent`'s view.
    Tab {
        agent: String,
        tab: AppTab,
    },
    /// Opens `agent`'s view, on its chat with `chat`.
    Agent {
        agent: String,
        chat: bool,
    },
    /// Sends a message. `open` then shows the chat.
    Say {
        agent: String,
        text: String,
        open: bool,
    },
    Copy(String),
    /// Opens a page in `agent`'s browser.
    Url {
        agent: String,
        url: String,
    },
    Notify(String),
    /// Sends `??`'s question to the daemon.
    AskQuick {
        agent: String,
        question: String,
    },
    /// Replaces the query, for example with a keyword and a space.
    Fill(String),
    Go(Place),
    /// Opens `commands.toml` in a text editor on `agent`'s desktop.
    EditCommands {
        agent: Option<String>,
    },
    Nothing,
}

/// One result.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    /// For the history of what ran. Empty rows are not remembered.
    pub id: String,
    pub glyph: Glyph,
    pub title: String,
    /// What Enter does, in words.
    pub subtitle: String,
    pub act: Act,
    /// Shift+Enter, with what it does in words.
    pub alt: Option<(String, Act)>,
    /// What Tab puts in the field.
    pub complete: Option<String>,
    pub score: f32,
    /// A longer text below the title, the `??` answer.
    pub body: Option<String>,
    pub error: bool,
}

impl Row {
    fn new(id: impl Into<String>, glyph: Glyph, title: impl Into<String>, subtitle: impl Into<String>, act: Act, score: f32) -> Self {
        Row {
            id: id.into(),
            glyph,
            title: title.into(),
            subtitle: subtitle.into(),
            act,
            alt: None,
            complete: None,
            score,
            body: None,
            error: false,
        }
    }

    fn alt(mut self, what: impl Into<String>, act: Act) -> Self {
        self.alt = Some((what.into(), act));
        self
    }

    fn complete(mut self, text: impl Into<String>) -> Self {
        self.complete = Some(text.into());
        self
    }

    fn error(text: impl Into<String>, score: f32) -> Self {
        Row { error: true, ..Row::new("", Glyph::Error, text, "Fix the file and the box loads it again", Act::Nothing, score) }
    }
}

/// The rows for `input`, best first.
pub fn rows(input: &Input) -> Vec<Row> {
    let q = input.query.trim_start();
    let mut rows = if let Some(rest) = q.strip_prefix("??") {
        quick_ask(input, rest.trim())
    } else if let Some(rest) = q.strip_prefix('?') {
        ask(input, rest.trim())
    } else if let Some(rest) = strip_word(q, "ask") {
        ask(input, rest)
    } else if let Some(rest) = q.strip_prefix('@') {
        talk(input, rest)
    } else if let Some(rest) = strip_word(q, "g") {
        web(input, rest)
    } else if let Some(rows) = keyword(input, q) {
        rows
    } else if q.trim().is_empty() {
        empty(input)
    } else {
        general(input, q.trim())
    };
    rows.sort_by(|a, b| b.score.total_cmp(&a.score));
    rows.truncate(MAX_ROWS);
    rows
}

/// `word rest` with the word typed in full and a space after it.
fn strip_word<'q>(q: &'q str, word: &str) -> Option<&'q str> {
    let (first, rest) = q.split_once(char::is_whitespace)?;
    first.eq_ignore_ascii_case(word).then(|| rest.trim_start())
}

fn assistant<'a>(input: &'a Input) -> Option<&'a AgentRef> {
    find_agent(input.agents, &input.settings.assistant)
}

/// An agent by id, by its name or by the start of its name.
pub fn find_agent<'a>(agents: &'a [AgentRef], who: &str) -> Option<&'a AgentRef> {
    let who = who.trim().to_lowercase();
    if who.is_empty() {
        return None;
    }
    agents
        .iter()
        .find(|a| a.id == who || a.name.to_lowercase() == who)
        .or_else(|| agents.iter().find(|a| a.name.to_lowercase().starts_with(&who) || a.id.starts_with(&who)))
}

/// "Claude's", or "Ross'" for a name ending in s.
pub fn possessive(name: &str) -> String {
    if name.ends_with('s') { format!("{name}'") } else { format!("{name}'s") }
}

fn recency(input: &Input, id: &str) -> f32 {
    let at = input.history.get(id).copied().unwrap_or(0);
    let at = match id.strip_prefix("app:") {
        Some(desktop) => at.max(input.app_recent.get(desktop).copied().unwrap_or(0)),
        None => at,
    };
    if at == 0 {
        return 0.0;
    }
    let hours = input.now.saturating_sub(at) as f32 / 3600.0;
    220.0 / (1.0 + hours / 24.0)
}

fn last_used(input: &Input, id: &str) -> u64 {
    let at = input.history.get(id).copied().unwrap_or(0);
    match id.strip_prefix("app:") {
        Some(desktop) => at.max(input.app_recent.get(desktop).copied().unwrap_or(0)),
        None => at,
    }
}

// ------------------------------------------------------------------ the LLM

fn no_daemon() -> Row {
    Row::error("The agent daemon is not running, so no agent can answer", 2000.0)
}

fn no_assistant(input: &Input) -> Row {
    let mut row = Row::error(format!("There is no agent called {} for questions", input.settings.assistant), 2000.0);
    row.subtitle = "Set [commands] assistant in meerora.toml to one of your agents".into();
    row
}

/// `? question` and `ask question`: the question goes to the assistant's chat.
fn ask(input: &Input, question: &str) -> Vec<Row> {
    let Some(agent) = assistant(input) else { return vec![no_assistant(input)] };
    if !input.daemon {
        return vec![no_daemon()];
    }
    if question.is_empty() {
        let row = Row::new("", Glyph::Ask, format!("Ask {}", agent.name), "Type your question after the ?", Act::Nothing, 2000.0);
        return vec![
            row.alt(format!("Shift+Enter opens {} chat", possessive(&agent.name)), Act::Agent { agent: agent.id.clone(), chat: true }),
        ];
    }
    let note = if agent.mock { format!(" · {} is a mock agent and answers with a note", agent.name) } else { String::new() };
    let row = Row::new(
        "",
        Glyph::Ask,
        format!("Ask {}: {question}", agent.name),
        format!("Enter sends it to {} and opens the chat{note}", agent.name),
        Act::Say { agent: agent.id.clone(), text: question.to_string(), open: true },
        2000.0,
    );
    vec![row.alt("Shift+Enter sends it and stays here", Act::Say { agent: agent.id.clone(), text: question.to_string(), open: false })]
}

/// `?? question`: a quick answer in the box. Enter asks, once the answer is
/// there Enter copies it.
fn quick_ask(input: &Input, question: &str) -> Vec<Row> {
    let Some(agent) = assistant(input) else { return vec![no_assistant(input)] };
    if !input.daemon {
        return vec![no_daemon()];
    }
    let chat = (
        format!("Shift+Enter asks in {} chat instead", possessive(&agent.name)),
        Act::Say { agent: agent.id.clone(), text: question.to_string(), open: true },
    );
    if question.is_empty() {
        let row =
            Row::new("", Glyph::Ask, format!("Quick answer from {}", agent.name), "Type your question after the ??", Act::Nothing, 2000.0);
        return vec![row];
    }
    let answer = input.answer.filter(|a| a.question == question && a.agent == agent.id);
    let mut row = match answer {
        None => Row::new(
            "",
            Glyph::Ask,
            format!("Ask {}: {question}", agent.name),
            format!("Enter asks {} for a quick answer here", agent.name),
            Act::AskQuick { agent: agent.id.clone(), question: question.to_string() },
            2000.0,
        ),
        Some(a) if !a.done => {
            let mut row = Row::new("", Glyph::Ask, question, format!("{} is answering · Esc stops it", agent.name), Act::Nothing, 2000.0);
            row.body = Some(if a.text.is_empty() { "…".into() } else { a.text.clone() });
            row
        }
        Some(a) => {
            let (subtitle, act) = match &a.error {
                Some(e) if a.text.is_empty() => (e.clone(), Act::AskQuick { agent: agent.id.clone(), question: question.to_string() }),
                Some(e) => (format!("{e} · Enter copies what came"), Act::Copy(a.text.clone())),
                None => (format!("{} answered · Enter copies the answer", agent.name), Act::Copy(a.text.clone())),
            };
            let mut row = Row::new("", Glyph::Ask, question, subtitle, act, 2000.0);
            row.error = a.error.is_some() && a.text.is_empty();
            row.body = (!a.text.is_empty()).then(|| a.text.clone());
            row
        }
    };
    row.alt = Some(chat);
    vec![row]
}

// ----------------------------------------------------------------- talking

/// `@name message` like the talk field, `@name` alone opens the view.
fn talk(input: &Input, rest: &str) -> Vec<Row> {
    let (who, message) = rest.split_once(char::is_whitespace).map_or((rest, ""), |(w, m)| (w, m.trim()));
    if !message.is_empty()
        && let Some(agent) = find_agent(input.agents, who)
    {
        if !input.daemon {
            return vec![no_daemon()];
        }
        if agent.mock {
            let row = Row::new(
                format!("agent:{}", agent.id),
                Glyph::Agent(agent.id.clone()),
                format!("{} is a mock agent", agent.name),
                format!("The demo scenario plays {}, Enter opens its view", agent.name),
                Act::Agent { agent: agent.id.clone(), chat: true },
                2000.0,
            );
            return vec![row];
        }
        let row = Row::new(
            "",
            Glyph::Agent(agent.id.clone()),
            format!("Send to {}: {message}", agent.name),
            format!("Enter sends it · {} answers in its chat", agent.name),
            Act::Say { agent: agent.id.clone(), text: message.to_string(), open: false },
            2000.0,
        );
        return vec![
            row.alt("Shift+Enter sends it and opens the chat", Act::Say { agent: agent.id.clone(), text: message.to_string(), open: true }),
        ];
    }
    let mut rows: Vec<Row> = input
        .agents
        .iter()
        .filter_map(|a| {
            let score = fuzzy::best(who, &[&a.name, &a.id], &[])?;
            let row = Row::new(
                format!("agent:{}", a.id),
                Glyph::Agent(a.id.clone()),
                a.name.clone(),
                format!("Enter opens {} view · Tab writes a message to {}", possessive(&a.name), a.name),
                Act::Agent { agent: a.id.clone(), chat: false },
                1000.0 + score,
            );
            Some(row.complete(format!("@{} ", a.id)))
        })
        .collect();
    if rows.is_empty() {
        rows.push(Row::error(format!("No agent called {who} in this space"), 2000.0));
        rows[0].subtitle = "Start with @ and an agent's name, for example @claude".into();
    }
    rows
}

// --------------------------------------------------------------------- web

fn web(input: &Input, query: &str) -> Vec<Row> {
    let Some(target) = input.target else { return vec![no_target("Search the web")] };
    if query.is_empty() {
        return vec![Row::new("", Glyph::Web, "Search the web", "Type what to search for after g", Act::Nothing, 2000.0)];
    }
    let url = input.settings.search.replace("{query}", &encode(query));
    let row = Row::new(
        "",
        Glyph::Web,
        format!("Search the web for {query}"),
        format!("Enter opens it in {} browser · {}", possessive(&target.name), host(&url)),
        Act::Url { agent: target.id.clone(), url: url.clone() },
        2000.0,
    );
    vec![row.alt("Shift+Enter copies the link", Act::Copy(url))]
}

fn no_target(title: &str) -> Row {
    let mut row = Row::error(title, 2000.0);
    row.subtitle = "There is no agent in this space to open it for".into();
    row
}

/// The host of a URL, for subtitles.
pub fn host(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    rest.split(['/', '?', '#']).next().unwrap_or(rest)
}

/// A typed URL: `https://…`, or a bare domain like `example.org/path`.
pub fn url_of(q: &str) -> Option<(String, bool)> {
    if q.contains(char::is_whitespace) || q.len() > 2000 {
        return None;
    }
    let lower = q.to_ascii_lowercase();
    if (lower.starts_with("https://") || lower.starts_with("http://")) && q.len() > 8 {
        return Some((q.to_string(), true));
    }
    let host = q.split(['/', '?', '#']).next()?;
    let (name, tld) = host.rsplit_once('.')?;
    let tld_ok = tld.len() >= 2 && tld.chars().all(|c| c.is_ascii_alphabetic());
    let name_ok = !name.is_empty() && name.split('.').all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
    (tld_ok && name_ok).then(|| (format!("https://{q}"), false))
}

// ---------------------------------------------------------------- keywords

/// The rows of a keyword of `commands.toml` or a script, when the query
/// starts with one and a space. None when it does not.
fn keyword(input: &Input, q: &str) -> Option<Vec<Row>> {
    let (word, rest) = q.split_once(char::is_whitespace)?;
    let rest = rest.trim();
    let mut rows = Vec::new();
    let mut found = false;
    if let Some(entry) = input.entries.iter().find(|e| e.keyword.eq_ignore_ascii_case(word)) {
        found = true;
        let action = entry.action(rest, &input.settings.assistant);
        let title = if entry.takes_query() && !rest.is_empty() { format!("{}: {rest}", entry.title) } else { entry.title.clone() };
        let mut row = action_row(
            input,
            format!("cmd:{}", entry.keyword),
            Glyph::Command(entry.keyword.clone()),
            title,
            &entry.subtitle,
            &action,
            2000.0,
        );
        row.complete = Some(format!("{} ", entry.keyword));
        rows.push(row);
    }
    for s in &input.scripts {
        let Some(meta) = s.meta.filter(|m| m.keyword.eq_ignore_ascii_case(word)) else { continue };
        found = true;
        match s.answer {
            None => rows.push(Row::new(
                "",
                Glyph::Script("js".into()),
                meta.title.clone(),
                format!("{} is working…", s.name),
                Act::Nothing,
                1900.0,
            )),
            Some((_, Err(e))) => {
                let mut row = Row::error(e.clone(), 1900.0);
                row.subtitle = format!("{} failed · fix the script and the box loads it again", s.name);
                rows.push(row);
            }
            Some((_, Ok(items))) => {
                if items.is_empty() {
                    rows.push(Row::new("", Glyph::Script("js".into()), meta.title.clone(), "No results", Act::Nothing, 1900.0));
                }
                for (i, item) in items.iter().enumerate() {
                    rows.push(script_row(input, s.name, item, 1900.0 - i as f32));
                }
            }
        }
    }
    found.then_some(rows)
}

fn script_row(input: &Input, name: &str, item: &CommandItem, score: f32) -> Row {
    let glyph = Glyph::Script(item.glyph.clone().unwrap_or_else(|| "js".into()));
    let mut row = match &item.action {
        Some(CommandAction::Calc { expression }) => match calc::eval(expression).or_else(|| calc::eval(&format!("({expression})+0"))) {
            Some(answer) => {
                let title = if item.title.is_empty() { format!("= {}", answer.text) } else { item.title.clone() };
                let subtitle = join(&item.subtitle, &format!("= {} · Enter copies it", answer.text));
                Row::new("", glyph, title, subtitle, Act::Copy(answer.text), score)
            }
            None => Row {
                error: true,
                ..Row::new(
                    "",
                    Glyph::Error,
                    format!("{name}: {expression:?} is no calculation"),
                    item.subtitle.clone(),
                    Act::Nothing,
                    score,
                )
            },
        },
        Some(action) => action_row(input, String::new(), glyph, item.title.clone(), &item.subtitle, action, score),
        None => Row::new("", glyph, item.title.clone(), item.subtitle.clone(), Act::Nothing, score),
    };
    if let Some(alt) = &item.alt {
        let (act, what) = act_of(input, alt);
        row.alt = Some((format!("Shift+Enter {what}"), act));
    }
    row
}

fn join(a: &str, b: &str) -> String {
    match (a.trim().is_empty(), b.trim().is_empty()) {
        (true, _) => b.to_string(),
        (false, true) => a.to_string(),
        (false, false) => format!("{a} · {b}"),
    }
}

/// A row for an action of `commands.toml` or a script. The subtitle always
/// says what Enter does, after the author's own subtitle, so a row cannot
/// hide what it runs (D8).
fn action_row(input: &Input, id: String, glyph: Glyph, title: String, subtitle: &str, action: &CommandAction, score: f32) -> Row {
    let (act, what) = act_of(input, action);
    let error = matches!(act, Act::Nothing) && !matches!(action, CommandAction::Calc { .. });
    let mut row = Row::new(id, glyph, title, join(subtitle, &format!("Enter {what}")), act, score);
    row.error = error;
    if error {
        row.glyph = Glyph::Error;
    }
    row
}

/// What a declarative action does here, and how to say it after "Enter".
pub fn act_of(input: &Input, action: &CommandAction) -> (Act, String) {
    let target = input.target;
    let on_target = |what: &str| target.map(|t| format!("{what} {}", possessive(&t.name)));
    match action {
        CommandAction::OpenUrl { url } => match target {
            Some(t) => {
                (Act::Url { agent: t.id.clone(), url: url.clone() }, format!("opens {} in {} browser", host(url), possessive(&t.name)))
            }
            None => (Act::Nothing, "has no agent whose browser could open it".into()),
        },
        CommandAction::Launch { program, args } => match target {
            Some(t) => {
                let line = std::iter::once(program.as_str()).chain(args.iter().map(String::as_str)).collect::<Vec<_>>().join(" ");
                let line = if line.chars().count() > 60 { format!("{}…", line.chars().take(59).collect::<String>()) } else { line };
                (
                    Act::Run { agent: t.id.clone(), program: program.clone(), args: args.clone(), show: true },
                    format!("runs {line} on {} desktop", possessive(&t.name)),
                )
            }
            None => (Act::Nothing, "has no agent desktop to run it on".into()),
        },
        CommandAction::Say { agent, text } => match find_agent(input.agents, agent) {
            Some(a) if input.daemon => {
                (Act::Say { agent: a.id.clone(), text: text.clone(), open: false }, format!("sends it to {}", a.name))
            }
            Some(_) => (Act::Nothing, "cannot send, the agent daemon is not running".into()),
            None => (Act::Nothing, format!("cannot send, there is no agent called {agent}")),
        },
        CommandAction::OpenApp { app } => match (AppTab::parse_handle(app).filter(|t| t.app().is_some()), target) {
            (Some(tab), Some(t)) => (
                Act::Tab { agent: t.id.clone(), tab },
                on_target(&format!("opens {} in", tab.label())).map(|s| format!("{s} view")).unwrap_or_default(),
            ),
            (None, _) => (Act::Nothing, format!("cannot open {app:?}, which is no built-in app")),
            (_, None) => (Act::Nothing, "has no agent view to open it in".into()),
        },
        CommandAction::Copy { text } => (Act::Copy(text.clone()), format!("copies {}", short(text, 40))),
        CommandAction::Notify { text } => (Act::Notify(text.clone()), "shows it as a note".into()),
        CommandAction::Calc { expression } => match calc::eval(expression).or_else(|| calc::eval(&format!("({expression})+0"))) {
            Some(a) => (Act::Copy(a.text.clone()), format!("copies {}", a.text)),
            None => (Act::Nothing, format!("cannot calculate {expression:?}")),
        },
    }
}

fn short(text: &str, max: usize) -> String {
    let one_line = text.replace(['\n', '\r'], " ");
    if one_line.chars().count() <= max {
        format!("\"{one_line}\"")
    } else {
        format!("\"{}…\"", one_line.chars().take(max - 1).collect::<String>())
    }
}

// ----------------------------------------------------------------- general

/// The empty query: what ran recently, then the agents and the prefixes.
fn empty(input: &Input) -> Vec<Row> {
    let mut rows: Vec<Row> = input.errors.iter().map(|e| Row::error(e.clone(), 5000.0)).collect();
    rows.extend(input.scripts.iter().filter_map(|s| s.error.map(|e| Row::error(e.to_string(), 4900.0))));
    let mut candidates = general_candidates(input, "");
    candidates.retain(|r| !r.id.is_empty() && last_used(input, &r.id) > 0);
    candidates.sort_by_key(|r| std::cmp::Reverse(last_used(input, &r.id)));
    candidates.truncate(6);
    for (i, mut row) in candidates.into_iter().enumerate() {
        row.score = 3000.0 - i as f32;
        rows.push(row);
    }
    let shown: Vec<String> = rows.iter().map(|r| r.id.clone()).collect();
    for (i, a) in input.agents.iter().enumerate().filter(|(_, a)| !shown.contains(&format!("agent:{}", a.id))).take(4) {
        rows.push(agent_row(a, 2000.0 - i as f32));
    }
    for (i, row) in prefix_rows(input).into_iter().enumerate() {
        rows.push(Row { score: 1000.0 - i as f32, ..row });
    }
    rows
}

fn general(input: &Input, q: &str) -> Vec<Row> {
    let mut rows = Vec::new();
    if let Some(answer) = calc::eval(q) {
        let subtitle = match &answer.note {
            Some(note) => format!("{note} · Enter copies {}", answer.text),
            None => format!("Enter copies {}", answer.text),
        };
        let row = Row::new("", Glyph::Math, format!("= {}", answer.text), subtitle, Act::Copy(answer.text.clone()), 5000.0);
        rows.push(row.alt("Shift+Enter puts the result in the field", Act::Fill(answer.text.clone())));
    }
    if let Some((url, sure)) = url_of(q) {
        rows.push(match input.target {
            Some(t) => Row::new(
                "",
                Glyph::Web,
                format!("Open {}", host(&url)),
                format!("Enter opens {url} in {} browser", possessive(&t.name)),
                Act::Url { agent: t.id.clone(), url: url.clone() },
                if sure { 4800.0 } else { 820.0 },
            )
            .alt("Shift+Enter copies the link", Act::Copy(url)),
            None => no_target("Open the page"),
        });
    }
    let errors = input.scripts.iter().filter_map(|s| {
        let stem = s.name.trim_end_matches(".js");
        let e = s.error?;
        fuzzy::score(q, stem).filter(|sc| *sc >= 600.0).map(|sc| Row::error(e.to_string(), sc + 100.0))
    });
    rows.extend(errors);
    for mut row in general_candidates(input, q) {
        row.score += recency(input, &row.id);
        rows.push(row);
    }
    if rows.is_empty() {
        rows.push(
            Row::new(
                "",
                Glyph::Ask,
                format!("Ask {} about \"{q}\"", assistant(input).map_or(input.settings.assistant.as_str(), |a| a.name.as_str())),
                "Nothing else matches · Tab writes ? and your text",
                Act::Fill(format!("? {q}")),
                10.0,
            )
            .complete(format!("? {q}")),
        );
        if let Some(t) = input.target {
            let url = input.settings.search.replace("{query}", &encode(q));
            rows.push(Row::new(
                "",
                Glyph::Web,
                format!("Search the web for {q}"),
                format!("Enter opens it in {} browser", possessive(&t.name)),
                Act::Url { agent: t.id.clone(), url },
                9.0,
            ));
        }
    }
    rows
}

/// Apps, agents, places, keywords and prefixes matching `q`, scored by the
/// match alone. An empty `q` gives all of them with score 0.
fn general_candidates(input: &Input, q: &str) -> Vec<Row> {
    let mut rows = Vec::new();
    let target = input.target;
    for app in App::ALL {
        let tab = AppTab::from_app(app);
        let Some(score) = fuzzy::best(q, &[tab.title()], &[tab.blurb()]) else { continue };
        let row = match target {
            Some(t) => {
                let row = Row::new(
                    format!("builtin:{}", tab.title().to_lowercase()),
                    Glyph::Builtin(app),
                    tab.title(),
                    format!("{} · Enter opens it in {} view", tab.blurb(), possessive(&t.name)),
                    Act::Builtin { agent: t.id.clone(), app, new: false },
                    score + 10.0,
                );
                if super::super::multi(app) {
                    row.alt(
                        format!("Shift+Enter opens another {}", tab.title().to_lowercase()),
                        Act::Builtin { agent: t.id.clone(), app, new: true },
                    )
                } else {
                    row
                }
            }
            None => Row::new(
                format!("builtin:{}", tab.title().to_lowercase()),
                Glyph::Builtin(app),
                tab.title(),
                "There is no agent to open it for",
                Act::Nothing,
                score,
            ),
        };
        rows.push(row);
    }
    for app in input.apps {
        let Some(score) = fuzzy::best(q, &[&app.name], &[&app.hints]) else { continue };
        let id = format!("app:{}", app.id);
        let row = match target {
            Some(t) => Row::new(
                id,
                Glyph::App(app.name.clone()),
                app.name.clone(),
                join(&app.blurb, &format!("Enter starts it on {} desktop", possessive(&t.name))),
                Act::Launch { app: app.clone(), agent: t.id.clone(), show: true },
                score,
            )
            .alt("Shift+Enter starts it and stays here", Act::Launch { app: app.clone(), agent: t.id.clone(), show: false }),
            None => Row::new(
                id,
                Glyph::App(app.name.clone()),
                app.name.clone(),
                "There is no agent desktop to start it on",
                Act::Nothing,
                score,
            ),
        };
        rows.push(row);
    }
    for a in input.agents {
        if let Some(score) = fuzzy::best(q, &[&a.name, &a.id], &[]) {
            rows.push(agent_row(a, score + 30.0));
        }
    }
    for (place, title, hints, what) in places() {
        if let Some(score) = fuzzy::best(q, &[&title], &[&hints]) {
            let id = match &place {
                Place::Room(id, _) => format!("room:{id}"),
                other => format!("place:{other:?}").to_lowercase(),
            };
            rows.push(Row::new(id, Glyph::Place, title, what, Act::Go(place), score));
        }
    }
    let settings_score = fuzzy::best(q, &["Command box settings"], &["commands.toml keywords configure scripts"]);
    if let Some(score) = settings_score {
        let what = match target {
            Some(t) => {
                format!("Enter opens commands.toml in a text editor on {} desktop · Shift+Enter copies its path", possessive(&t.name))
            }
            None => "Enter copies the path of commands.toml".into(),
        };
        let row = Row::new(
            "place:settings",
            Glyph::Place,
            "Command box settings",
            what,
            Act::EditCommands { agent: target.map(|t| t.id.clone()) },
            score,
        );
        rows.push(row.alt("Shift+Enter copies the path of commands.toml", Act::EditCommands { agent: None }));
    }
    for entry in input.entries {
        let Some(score) = fuzzy::best(q, &[&entry.keyword, &entry.title], &[&entry.subtitle]) else { continue };
        let score = keyword_score(q, &entry.keyword, score);
        let id = format!("cmd:{}", entry.keyword);
        let row = if entry.takes_query() {
            Row::new(
                id,
                Glyph::Command(entry.keyword.clone()),
                entry.title.clone(),
                join(&entry.subtitle, &format!("Type {} and your text · Enter writes the keyword", entry.keyword)),
                Act::Fill(format!("{} ", entry.keyword)),
                score,
            )
        } else {
            let action = entry.action("", &input.settings.assistant);
            action_row(input, id, Glyph::Command(entry.keyword.clone()), entry.title.clone(), &entry.subtitle, &action, score)
        };
        rows.push(row.complete(format!("{} ", entry.keyword)));
    }
    for s in &input.scripts {
        let Some(meta) = s.meta else { continue };
        let Some(score) = fuzzy::best(q, &[&meta.keyword, &meta.title], &[&meta.subtitle]) else { continue };
        let score = keyword_score(q, &meta.keyword, score);
        let hint = format!("Type {} and your text · Enter writes the keyword", meta.keyword);
        let row = Row::new(
            format!("script:{}", meta.keyword),
            Glyph::Script("js".into()),
            meta.title.clone(),
            join(&meta.subtitle, &hint),
            Act::Fill(format!("{} ", meta.keyword)),
            score,
        );
        rows.push(row.complete(format!("{} ", meta.keyword)));
    }
    if !q.is_empty() {
        for row in prefix_rows(input) {
            let words = format!("{} {}", row.title, row.complete.clone().unwrap_or_default());
            if let Some(score) = fuzzy::best(q, &[&row.title], &[&words]) {
                rows.push(Row { score: score * 0.9, ..row });
            }
        }
    }
    rows
}

/// Keywords are short, so a few letters cover them almost fully. Below an
/// exact keyword they rank a little under apps and agents with the same match.
fn keyword_score(q: &str, keyword: &str, score: f32) -> f32 {
    if q.trim().eq_ignore_ascii_case(keyword) { score + 100.0 } else { score * 0.9 }
}

fn agent_row(a: &AgentRef, score: f32) -> Row {
    Row::new(
        format!("agent:{}", a.id),
        Glyph::Agent(a.id.clone()),
        a.name.clone(),
        format!("Enter opens {} view", possessive(&a.name)),
        Act::Agent { agent: a.id.clone(), chat: false },
        score,
    )
    .alt(format!("Shift+Enter opens {} chat", possessive(&a.name)), Act::Agent { agent: a.id.clone(), chat: true })
    .complete(format!("@{} ", a.id))
}

/// The places of meerora: title, words that find it, and what Enter does.
fn places() -> Vec<(Place, String, String, String)> {
    let mut out = vec![
        (Place::Space, "Space".to_string(), "home back shared room overview".to_string(), "Enter goes back to the space".to_string()),
        (Place::Feed, "Feed".into(), "posts news digest friends social".into(), "Enter opens the feed".into()),
        (Place::Graveyard, "Graveyard".into(), "retired agents bring back".into(), "Enter opens the graveyard".into()),
        (
            Place::Rooms,
            "Choose a room".into(),
            "room space workshop station office".into(),
            "Enter opens the room picker in the space".into(),
        ),
        (Place::NewAgent, "New agent".into(), "builder create make figure".into(), "Enter opens the builder".into()),
        (
            Place::Index,
            "Get an external agent".into(),
            "index import search company person".into(),
            "Enter searches the meerora index".into(),
        ),
    ];
    for t in meerora_protocol::TEMPLATES {
        out.push((
            Place::Room(t.id.to_string(), t.name.to_string()),
            format!("Room: {}", t.name),
            format!("room switch {}", t.description),
            format!("Enter makes the space the {}", t.name.to_lowercase()),
        ));
    }
    out
}

/// The prefixes of the box itself, as rows that write them into the field.
fn prefix_rows(input: &Input) -> Vec<Row> {
    let name = assistant(input).map_or(input.settings.assistant.clone(), |a| a.name.clone());
    vec![
        Row::new(
            "",
            Glyph::Ask,
            format!("Ask {name}"),
            format!("? and a question · {name} answers in the chat"),
            Act::Fill("? ".into()),
            0.0,
        )
        .complete("? "),
        Row::new(
            "",
            Glyph::Ask,
            format!("Quick answer from {name}"),
            "?? and a question · the answer shows here".to_string(),
            Act::Fill("?? ".into()),
            0.0,
        )
        .complete("?? "),
        Row::new("", Glyph::Web, "Search the web", "g and what to search for".to_string(), Act::Fill("g ".into()), 0.0).complete("g "),
        Row::new(
            "",
            Glyph::Agent(String::new()),
            "Talk to an agent",
            "@ and a name, then a message".to_string(),
            Act::Fill("@".into()),
            0.0,
        )
        .complete("@"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agents() -> Vec<AgentRef> {
        vec![
            AgentRef { id: "claude".into(), name: "Claude".into(), color: [1, 2, 3], mock: false },
            AgentRef { id: "ember".into(), name: "Ember".into(), color: [1, 2, 3], mock: true },
        ]
    }

    fn app(id: &str, name: &str, hints: &str) -> DesktopApp {
        DesktopApp { id: id.into(), name: name.into(), blurb: String::new(), hints: hints.into(), exec: vec![id.into()] }
    }

    struct Fixture {
        agents: Vec<AgentRef>,
        settings: BoxSettings,
        entries: Vec<Entry>,
        apps: Vec<DesktopApp>,
        app_recent: HashMap<String, u64>,
        history: HashMap<String, u64>,
        answer: Option<Answer>,
    }

    impl Fixture {
        fn new() -> Self {
            let (entries, errors) = super::super::config::parse_entries(super::super::config::EXAMPLE_TOML);
            assert!(errors.is_empty());
            Fixture {
                agents: agents(),
                settings: BoxSettings::default(),
                entries,
                apps: vec![
                    app("org.gnome.TextEditor.desktop", "Text Editor", "Edit text files"),
                    app("org.gnome.Terminal.desktop", "Terminal", "Command line"),
                    app("firefox.desktop", "Firefox", "Web Browser internet"),
                    app("org.gnome.Settings.desktop", "Settings", "Preferences"),
                ],
                app_recent: HashMap::new(),
                history: HashMap::new(),
                answer: None,
            }
        }

        fn rows(&self, query: &str) -> Vec<Row> {
            let input = Input {
                query,
                agents: &self.agents,
                target: self.agents.first(),
                settings: &self.settings,
                entries: &self.entries,
                errors: &[],
                apps: &self.apps,
                app_recent: &self.app_recent,
                history: &self.history,
                scripts: Vec::new(),
                answer: self.answer.as_ref(),
                daemon: true,
                now: 1_000_000,
            };
            rows(&input)
        }

        fn titles(&self, query: &str) -> Vec<String> {
            self.rows(query).into_iter().map(|r| r.title).collect()
        }
    }

    #[test]
    fn apps_rank_by_match_then_by_recent_use() {
        let mut f = Fixture::new();
        let titles = f.titles("te");
        assert!(titles[0] == "Terminal" || titles[0] == "Text Editor", "{titles:?}");
        assert!(titles.iter().position(|t| t == "Settings").is_none_or(|p| p > 2), "{titles:?}");
        // The launcher's recent list and the box's own history put an app first.
        f.app_recent.insert("org.gnome.TextEditor.desktop".into(), 1_000_000 - 60);
        assert_eq!(f.titles("te")[0], "Text Editor");
        f.history.insert("app:firefox.desktop".into(), 1_000_000 - 30);
        assert_eq!(f.titles("f")[0], "Firefox");
        let row = &f.rows("firefox")[0];
        assert_eq!(row.subtitle, "Enter starts it on Claude's desktop");
        assert!(matches!(&row.act, Act::Launch { agent, show: true, .. } if agent == "claude"));
        assert!(matches!(&row.alt, Some((_, Act::Launch { show: false, .. }))));
        // A hint word finds it too, below name matches.
        assert_eq!(f.titles("browser")[0], "Browser", "the built-in browser by name before Firefox by keyword");
        assert!(f.titles("browser").contains(&"Firefox".to_string()));
    }

    #[test]
    fn the_empty_query_shows_recent_items_then_agents() {
        let mut f = Fixture::new();
        f.history.insert("agent:ember".into(), 1_000_000 - 10);
        f.history.insert("builtin:terminal".into(), 1_000_000 - 100);
        let rows = f.rows("");
        assert_eq!(rows[0].title, "Ember");
        assert_eq!(rows[1].title, "Terminal");
        assert_eq!(rows[2].title, "Claude", "the agents follow");
        assert!(rows.iter().any(|r| r.complete.as_deref() == Some("?? ")), "the prefixes are listed");
    }

    #[test]
    fn math_goes_first_and_copies() {
        let f = Fixture::new();
        let rows = f.rows("2+2*3");
        assert_eq!(rows[0].title, "= 8");
        assert_eq!(rows[0].act, Act::Copy("8".into()));
        assert_eq!(rows[0].alt.as_ref().unwrap().1, Act::Fill("8".into()));
        assert_eq!(f.rows("0x1f + 0b101")[0].subtitle, "0x24 · 0b100100 · Enter copies 36");
    }

    #[test]
    fn prefixes_pick_their_provider() {
        let mut f = Fixture::new();
        let ask = &f.rows("? what changed")[0];
        assert_eq!(ask.act, Act::Say { agent: "claude".into(), text: "what changed".into(), open: true });
        assert_eq!(f.rows("ask what changed")[0].act, ask.act);
        assert_eq!(f.rows("?? capital of France")[0].act, Act::AskQuick { agent: "claude".into(), question: "capital of France".into() });

        f.answer = Some(Answer {
            request: "r".into(),
            agent: "claude".into(),
            question: "capital of France".into(),
            text: "Paris".into(),
            done: false,
            error: None,
        });
        let pending = &f.rows("?? capital of France")[0];
        assert_eq!((pending.body.as_deref(), &pending.act), (Some("Paris"), &Act::Nothing));
        f.answer.as_mut().unwrap().done = true;
        let done = &f.rows("?? capital of France")[0];
        assert_eq!(done.act, Act::Copy("Paris".into()));
        assert_eq!(f.rows("?? something else")[0].act, Act::AskQuick { agent: "claude".into(), question: "something else".into() });

        let talk = &f.rows("@cl hello there")[0];
        assert_eq!(talk.act, Act::Say { agent: "claude".into(), text: "hello there".into(), open: false });
        assert!(f.rows("@em hi")[0].title.contains("mock"));
        assert_eq!(f.rows("@cl")[0].complete.as_deref(), Some("@claude "));

        let web = &f.rows("g rust borrow checker")[0];
        assert_eq!(web.act, Act::Url { agent: "claude".into(), url: "https://duckduckgo.com/?q=rust%20borrow%20checker".into() });
        let url = &f.rows("https://example.org/a?b=c")[0];
        assert_eq!(url.act, Act::Url { agent: "claude".into(), url: "https://example.org/a?b=c".into() });
        assert!(matches!(&f.rows("example.org")[0].act, Act::Url { url, .. } if url == "https://example.org"));

        f.settings.assistant = "nobody".into();
        assert!(f.rows("? hi")[0].error);
    }

    #[test]
    fn keywords_run_and_complete() {
        let f = Fixture::new();
        let w = &f.rows("w rust lang")[0];
        assert_eq!(w.title, "Search Wikipedia: rust lang");
        assert_eq!(w.act, Act::Url { agent: "claude".into(), url: "https://en.wikipedia.org/w/index.php?search=rust%20lang".into() });
        assert!(w.subtitle.contains("Enter opens en.wikipedia.org in Claude's browser"), "{}", w.subtitle);
        // Typing a keyword without the space offers it, and Enter or Tab writes it.
        let suggestion = f.rows("w").into_iter().find(|r| r.id == "cmd:w").unwrap();
        assert_eq!((suggestion.act, suggestion.complete.as_deref()), (Act::Fill("w ".into()), Some("w ")));
        // An entry without {query} runs at once.
        let shrug = f.rows("shrug").into_iter().find(|r| r.id == "cmd:shrug").unwrap();
        assert_eq!(shrug.act, Act::Copy("¯\\_(ツ)_/¯".into()));
        let launch = &f.rows("calc ")[0];
        assert!(matches!(&launch.act, Act::Run { program, .. } if program == "gnome-calculator"));
        assert!(launch.subtitle.contains("runs gnome-calculator on Claude's desktop"), "{}", launch.subtitle);
    }

    #[test]
    fn script_items_become_rows() {
        let f = Fixture::new();
        let meta = CommandMeta { keyword: "temp".into(), title: "Temperatures".into(), subtitle: String::new() };
        let items = vec![
            CommandItem {
                title: "69.8 °F".into(),
                subtitle: "21 °C".into(),
                glyph: Some("°".into()),
                action: Some(CommandAction::Copy { text: "69.8".into() }),
                alt: Some(CommandAction::OpenUrl { url: "https://example.org".into() }),
            },
            CommandItem {
                title: String::new(),
                subtitle: String::new(),
                glyph: None,
                action: Some(CommandAction::Calc { expression: "21*9/5+32".into() }),
                alt: None,
            },
            CommandItem {
                title: "Run".into(),
                subtitle: String::new(),
                glyph: None,
                action: Some(CommandAction::Launch { program: "echo".into(), args: vec!["hi".into()] }),
                alt: None,
            },
            CommandItem {
                title: "App".into(),
                subtitle: String::new(),
                glyph: None,
                action: Some(CommandAction::OpenApp { app: "nope".into() }),
                alt: None,
            },
        ];
        let answer = ("21".to_string(), Ok(items));
        let input = Input {
            query: "temp 21",
            agents: &f.agents,
            target: f.agents.first(),
            settings: &f.settings,
            entries: &[],
            errors: &[],
            apps: &[],
            app_recent: &f.app_recent,
            history: &f.history,
            scripts: vec![ScriptView { name: "units.js", meta: Some(&meta), error: None, answer: Some(&answer) }],
            answer: None,
            daemon: true,
            now: 0,
        };
        let rows = rows(&input);
        assert_eq!(rows[0].title, "69.8 °F");
        assert_eq!(rows[0].glyph, Glyph::Script("°".into()));
        assert_eq!(rows[0].subtitle, "21 °C · Enter copies \"69.8\"");
        assert_eq!(rows[0].alt.as_ref().unwrap().0, "Shift+Enter opens example.org in Claude's browser");
        assert_eq!((rows[1].title.as_str(), &rows[1].act), ("= 69.8", &Act::Copy("69.8".into())));
        assert!(rows[2].subtitle.contains("runs echo hi on Claude's desktop"));
        assert!(rows[3].error, "an unknown app is shown as a problem");

        let failed = ("21".to_string(), Err("units.js:3: TypeError: boom".to_string()));
        let input =
            Input { scripts: vec![ScriptView { name: "units.js", meta: Some(&meta), error: None, answer: Some(&failed) }], ..input };
        let rows = super::rows(&input);
        assert!(rows[0].error && rows[0].title.contains("boom"));
    }

    #[test]
    fn urls_are_told_apart_from_words() {
        assert_eq!(url_of("example.org"), Some(("https://example.org".into(), false)));
        assert_eq!(url_of("http://localhost:8080/x"), Some(("http://localhost:8080/x".into(), true)));
        assert_eq!(url_of("docs.rs/egui/latest"), Some(("https://docs.rs/egui/latest".into(), false)));
        for words in ["firefox", "two words.com", "1.5", "a.b1", ".org", "x."] {
            assert_eq!(url_of(words), None, "{words:?}");
        }
        assert_eq!(host("https://en.wikipedia.org/w/index.php?x"), "en.wikipedia.org");
    }
}
