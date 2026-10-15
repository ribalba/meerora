//! The Git app as agents use it (D30), on the hooks of ui/apps/git.rs:
//! read returns the repository, the branch, the files by section and the
//! shown file's hunks, and `stage {path}`, `unstage {path}` and
//! `commit {message}` run git on a thread. Pushing stays with the user.

use serde_json::{Map, Value};

use super::{Acted, AgentApp, AppText, Element, Reply};
use crate::engine::Core;

pub struct GitApp {
    pub agent: String,
    pub n: u32,
}

impl AgentApp for GitApp {
    fn kind(&self) -> &'static str {
        "git"
    }

    fn actions(&self) -> &'static [&'static str] {
        &["stage", "unstage", "commit"]
    }

    fn read(&mut self, core: &mut Core) -> Reply<AppText> {
        let Some(v) = crate::ui::apps::git::read(core, &self.agent, self.n) else {
            return Reply::err(format!("git:{} has not loaded its repository yet. Try again once the user's view showed it.", self.n));
        };
        let text = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
        let elements = v
            .get("elements")
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .map(|e| {
                        let field = |k: &str| e.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
                        Element {
                            r#ref: field("ref"),
                            role: field("role"),
                            name: field("name"),
                            value: e.get("value").and_then(Value::as_str).map(String::from),
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
        let extra = v.get("extra").and_then(Value::as_object).cloned().unwrap_or_default();
        Reply::ok(AppText {
            kind: "git".into(),
            title: text("title"),
            summary: text("summary"),
            extra,
            elements,
            content: text("content"),
            ..Default::default()
        })
    }

    fn act(&mut self, core: &mut Core, action: &str, args: &Map<String, Value>) -> Reply<Acted> {
        let Some(git) = crate::ui::apps::git::repository(core, &self.agent, self.n) else {
            return Reply::err(format!("git:{} has no repository open.", self.n));
        };
        let field = |k: &str| args.get(k).and_then(Value::as_str).unwrap_or_default().trim().to_string();
        let summary = match action {
            "commit" => field("message").lines().next().unwrap_or_default().to_string(),
            _ => field("path"),
        };
        let (action, args, handle) = (action.to_string(), Value::Object(args.clone()), format!("git:{}", self.n));
        Reply::thread(move || {
            let done = crate::ui::apps::git::act(&git, &action, &args)?;
            Ok(Acted::new(format!("{handle}: {done}."), summary))
        })
    }
}
