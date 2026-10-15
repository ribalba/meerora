//! The chat, Changes and Activity as agents read them (D30): summaries of
//! the conversation, the git history of the agent's directory and the runs,
//! schedule and actions. They take no actions besides focus and close.

use meerora_protocol::Role;
use serde_json::{Map, Value, json};

use super::{Acted, AgentApp, AppText, Reply};
use crate::engine::Core;

/// Messages of the chat that `app_read` returns.
const MESSAGES: usize = 30;
/// Commits of Changes that `app_read` returns.
const COMMITS: usize = 30;
/// Runs and actions of Activity that `app_read` returns.
const RUNS: usize = 15;
const ACTIONS: usize = 30;

fn none(kind: &str, action: &str) -> Reply<Acted> {
    Reply::err(format!("The {kind} takes no action {action:?}. Read it with app_read."))
}

fn when(unix_seconds: i64) -> String {
    jiff::Timestamp::from_second(unix_seconds)
        .map(|t| t.to_zoned(jiff::tz::TimeZone::system()).strftime("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_default()
}

pub struct ChatApp {
    pub agent: String,
}

impl AgentApp for ChatApp {
    fn kind(&self) -> &'static str {
        "chat"
    }

    fn actions(&self) -> &'static [&'static str] {
        &[]
    }

    fn read(&mut self, core: &mut Core) -> Reply<AppText> {
        let Some(a) = core.world.agent(&self.agent) else { return Reply::err("This agent is gone.") };
        let lines: Vec<String> = a
            .messages
            .iter()
            .rev()
            .take(MESSAGES)
            .rev()
            .map(|m| {
                let who = match m.role {
                    Role::User => "user",
                    Role::Agent => "you",
                    Role::System => "meerora",
                };
                format!("{who}: {}", m.text.trim())
            })
            .collect();
        let mut extra = Map::new();
        if let Some(run) = &a.run {
            extra.insert("run".into(), json!(run.task.lines().next().unwrap_or_default()));
        }
        extra.insert("activity".into(), json!(a.activity));
        let summary = format!("The last {} messages of your conversation with the user, oldest first.", lines.len());
        Reply::ok(AppText {
            kind: "chat".into(),
            title: "Chat".into(),
            summary,
            extra,
            content: lines.join("\n\n"),
            keep_end: true,
            ..Default::default()
        })
    }

    fn act(&mut self, _core: &mut Core, action: &str, _args: &Map<String, Value>) -> Reply<Acted> {
        none("chat", action)
    }
}

pub struct ChangesApp {
    pub agent: String,
}

impl AgentApp for ChangesApp {
    fn kind(&self) -> &'static str {
        "changes"
    }

    fn actions(&self) -> &'static [&'static str] {
        &[]
    }

    fn read(&mut self, core: &mut Core) -> Reply<AppText> {
        let Some(dir) = core.agent_dir(&self.agent).map(|d| d.path.clone()) else { return Reply::err("This agent has no directory.") };
        Reply::thread(move || {
            let history = crate::ui::apps::changes::load_history(&dir)?;
            let lines: Vec<String> = history
                .entries
                .iter()
                .take(COMMITS)
                .map(|e| {
                    let (added, removed) = e.files.iter().fold((0, 0), |(a, r), f| (a + f.added.unwrap_or(0), r + f.removed.unwrap_or(0)));
                    let mut flags = Vec::new();
                    if !e.reviewed {
                        flags.push("not reviewed");
                    }
                    if e.undone_by.is_some() {
                        flags.push("undone");
                    }
                    let flags = if flags.is_empty() { String::new() } else { format!(" [{}]", flags.join(", ")) };
                    let files: Vec<&str> = e.files.iter().map(|f| f.path.as_str()).collect();
                    format!(
                        "{} {} {}: +{added} -{removed} in {}{flags}",
                        &e.id[..e.id.len().min(7)],
                        when(e.time),
                        e.title,
                        files.join(", ")
                    )
                })
                .collect();
            let review = history.entries.iter().filter(|e| !e.reviewed && e.undone_by.is_none()).count();
            let mut extra = Map::new();
            extra.insert("not_reviewed".into(), json!(review));
            let summary =
                format!("The git history of your directory, newest first, one commit per run. {review} not reviewed by the user.");
            Ok(AppText { kind: "changes".into(), title: "Changes".into(), summary, extra, content: lines.join("\n"), ..Default::default() })
        })
    }

    fn act(&mut self, _core: &mut Core, action: &str, _args: &Map<String, Value>) -> Reply<Acted> {
        none("changes app", action)
    }
}

pub struct ActivityApp {
    pub agent: String,
}

impl AgentApp for ActivityApp {
    fn kind(&self) -> &'static str {
        "activity"
    }

    fn actions(&self) -> &'static [&'static str] {
        &[]
    }

    fn read(&mut self, core: &mut Core) -> Reply<AppText> {
        let Some(dir) = core.agent_dir(&self.agent) else { return Reply::err("This agent has no directory.") };
        let config = dir.config.clone();
        let files = crate::ui::apps::activity::read_files(&dir.path);
        let now = core.world.time;
        let actions: Vec<String> = core
            .world
            .agent(&self.agent)
            .map(|a| a.actions.iter().rev().take(ACTIONS).map(|e| format!("{:.0} s ago  {} {}", now - e.at, e.tool, e.summary)).collect())
            .unwrap_or_default();
        let mut lines = vec!["Runs, newest first:".to_string()];
        for r in files.runs.iter().take(RUNS) {
            let summary = r.summary.as_deref().unwrap_or("");
            lines.push(format!(
                "- {} {:?} {:?}: {} {summary}",
                when((r.start / 1000) as i64),
                r.trigger,
                r.outcome,
                r.task.lines().next().unwrap_or("")
            ));
        }
        lines.push(String::new());
        lines.push("Schedule:".into());
        if config.schedule.is_empty() {
            lines.push("- none".into());
        }
        for s in &config.schedule {
            lines.push(format!("- {}: {}", crate::ui::apps::activity::when_text(s), s.task.lines().next().unwrap_or("")));
        }
        lines.push(String::new());
        lines.push("Your actions on the user's apps, newest first:".into());
        lines.extend(actions);
        let mut extra = Map::new();
        if let Some((day, used)) = &files.tokens {
            extra.insert("tokens_today".into(), json!({ "day": day, "used": used }));
        }
        if let Some(limit) = config.budget.as_ref().and_then(|b| b.tokens_per_day) {
            extra.insert("tokens_per_day".into(), json!(limit));
        }
        let summary = format!("Your runs, schedule, budget and actions. {} runs recorded.", files.runs.len());
        Reply::ok(AppText {
            kind: "activity".into(),
            title: "Activity".into(),
            summary,
            extra,
            content: lines.join("\n"),
            ..Default::default()
        })
    }

    fn act(&mut self, _core: &mut Core, action: &str, _args: &Map<String, Value>) -> Reply<Acted> {
        none("activity app", action)
    }
}
