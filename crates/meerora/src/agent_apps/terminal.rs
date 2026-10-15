//! The terminal as agents use it (D30): read returns the screen and the
//! recent scrollback, `type {text}` and `key {keys}` write to its PTY and
//! `run {command}` types a command, presses Enter and answers once the
//! output settled, with the new end of the screen.
//!
//! The terminal runs the shell of a terminal tile, which is your login
//! shell in the agent's directory, not the agent's sandbox. That is why
//! acting on it needs `apps.control`, which the user grants on purpose.

use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};

use super::{Acted, AgentApp, AppText, Reply, keys, text_arg};
use crate::engine::Core;

/// Lines of scrollback that `app_read` returns above the screen.
const SCROLLBACK: usize = 200;
/// `run` answers once the screen did not change for this long.
const QUIET: Duration = Duration::from_millis(500);
/// `run` answers after this long at the latest, the command may go on.
const RUN_WAIT: Duration = Duration::from_secs(8);
/// Lines of the screen's end that `run` returns.
const RUN_TAIL: usize = 30;
/// Longest `type` text, in characters.
const TYPE_MAX: usize = 10_000;

pub struct TerminalApp {
    pub agent: String,
    pub n: u32,
}

impl TerminalApp {
    fn handle(&self) -> String {
        format!("terminal:{}", self.n)
    }
}

/// The text of the terminal's screen, its rows trimmed and without the empty end.
fn screen_text(snap: &meerora_term::Snapshot) -> String {
    let cols = usize::from(snap.size.cols).max(1);
    let rows: Vec<String> = snap.cells.chunks(cols).map(|row| row.iter().filter(|c| !c.spacer).map(|c| c.ch).collect::<String>()).collect();
    let mut lines: Vec<String> = rows.into_iter().map(|r| r.trim_end().to_string()).collect();
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

fn tail(text: &str, lines: usize) -> String {
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

impl AgentApp for TerminalApp {
    fn kind(&self) -> &'static str {
        "terminal"
    }

    fn actions(&self) -> &'static [&'static str] {
        &["type", "key", "run"]
    }

    fn read(&mut self, core: &mut Core) -> Reply<AppText> {
        let dir = core.agent_dir(&self.agent).map(|d| d.path.display().to_string());
        let handle = self.handle();
        let Some(tile) = core.terminal(&self.agent, self.n) else {
            return Reply::err(format!("{handle} could not start its shell."));
        };
        let snap = tile.term.snapshot();
        let content = tile.term.text(SCROLLBACK);
        let alive = tile.term.is_alive();
        let title = snap.title.clone().filter(|t| !t.is_empty()).unwrap_or_else(|| format!("Terminal {}", self.n));
        let cursor = snap.cursor.map(|(col, row)| json!({ "row": row + 1, "column": col + 1 }));
        let mut extra = Map::new();
        if let Some(dir) = dir {
            extra.insert("started_in".into(), json!(dir));
        }
        extra.insert("size".into(), json!(format!("{} columns, {} rows", snap.size.cols, snap.size.rows)));
        if let Some(cursor) = cursor {
            extra.insert("cursor".into(), cursor);
        }
        let summary = if alive {
            format!("The shell of {handle}, with the screen and up to {SCROLLBACK} lines of scrollback above it.")
        } else {
            format!("The shell of {handle} has ended. Closing and opening the terminal starts a new one.")
        };
        Reply::ok(AppText { kind: "terminal".into(), title, summary, extra, content, keep_end: true, ..Default::default() })
    }

    fn act(&mut self, core: &mut Core, action: &str, args: &Map<String, Value>) -> Reply<Acted> {
        let handle = self.handle();
        let Some(tile) = core.terminal(&self.agent, self.n) else {
            return Reply::err(format!("{handle} could not start its shell."));
        };
        if !tile.term.is_alive() {
            return Reply::err(format!("The shell of {handle} has ended. Close it and open a new terminal with app_open."));
        }
        match action {
            "type" => {
                let text = match text_arg(args, "text", "type") {
                    Ok(t) => t,
                    Err(e) => return Reply::err(e),
                };
                if text.chars().count() > TYPE_MAX {
                    return Reply::err(format!("type takes at most {TYPE_MAX} characters at once."));
                }
                // Enter is a carriage return on a terminal.
                let bytes = text.replace("\r\n", "\r").replace('\n', "\r");
                tile.term.write(bytes.as_bytes());
                Reply::ok(Acted::new(format!("Typed {} characters into {handle}.", text.chars().count()), text))
            }
            "key" => {
                let keys_text = match text_arg(args, "keys", "key") {
                    Ok(t) => t,
                    Err(e) => return Reply::err(e),
                };
                let app_cursor = tile.term.snapshot().app_cursor;
                let combos = match keys::parse(&keys_text) {
                    Ok(c) => c,
                    Err(e) => return Reply::err(e),
                };
                let mut bytes = Vec::new();
                for combo in &combos {
                    match keys::terminal_bytes(combo, app_cursor) {
                        Ok(b) => bytes.extend(b),
                        Err(e) => return Reply::err(e),
                    }
                }
                tile.term.write(&bytes);
                Reply::ok(Acted::new(format!("Pressed {} in {handle}.", keys_text.trim()), keys_text.trim()))
            }
            "run" => {
                let command = match text_arg(args, "command", "run") {
                    Ok(t) => t.trim().to_string(),
                    Err(e) => return Reply::err(e),
                };
                if command.is_empty() {
                    return Reply::err("The command is empty.");
                }
                if command.contains(['\n', '\r']) {
                    return Reply::err("run takes one line. Run the lines one by one, or type them with type.");
                }
                let before = screen_text(&tile.term.snapshot());
                tile.term.write(format!("{command}\r").as_bytes());
                let (agent, n) = (self.agent.clone(), self.n);
                let started = Instant::now();
                let mut last = (before, Instant::now());
                Reply::Later(Box::new(move |core: &mut Core| {
                    let tile = core.terminal(&agent, n)?;
                    let now = screen_text(&tile.term.snapshot());
                    if now != last.0 {
                        last = (now.clone(), Instant::now());
                    }
                    let settled = last.1.elapsed() >= QUIET && started.elapsed() >= QUIET;
                    let waited = started.elapsed() >= RUN_WAIT;
                    if !(settled || waited || !tile.term.is_alive()) {
                        return None;
                    }
                    let state = if waited && !settled { " It is still running." } else { "" };
                    let text = format!("Ran `{command}` in {handle}.{state} The screen now ends with:\n{}", tail(&now, RUN_TAIL));
                    Some(Ok(Acted::new(text, command.clone())))
                }))
            }
            other => Reply::err(format!("A terminal has no action {other:?}.")),
        }
    }

    fn taken_over(&self, core: &mut Core) -> bool {
        core.terminal(&self.agent, self.n).is_some_and(|t| t.taken_over)
    }
}
