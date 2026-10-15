//! A browser page as agents use it (D30), over CDP through
//! `meerora_browser::agent`: read returns the URL, the title, the page text
//! and its links, buttons and inputs with refs, the actions open, click,
//! type, scroll, back, forward and reload drive the page, and the
//! screenshot is `Page.captureScreenshot`. The work runs on a thread, so
//! the UI never waits for Chrome.
//!
//! An agent opens only what `url::for_agent` allows: http, https and data
//! URLs, and files inside its own directory.

use std::time::{Duration, Instant};

use meerora_browser::{Page, Remote, agent};
use serde_json::{Map, Value, json};

use super::{Acted, AgentApp, AppText, Element, Reply, bool_arg, number_arg, text_arg};
use crate::engine::Core;

/// How long one step of the work may take in Chrome.
const WAIT: Duration = Duration::from_secs(12);
/// How long a page may take to load after a click or a submit.
const LOAD_WAIT: Duration = Duration::from_secs(8);

pub struct BrowserApp {
    pub agent: String,
    pub n: u32,
}

impl BrowserApp {
    fn handle(&self) -> String {
        format!("browser:{}", self.n)
    }

    fn remote(&self, core: &mut Core) -> Result<Remote, String> {
        crate::ui::browser::agent_page(core, &self.agent, self.n)
    }
}

/// Waits until a navigation that may follow an action has loaded, at most
/// [`LOAD_WAIT`]. Nothing happens when the page stays where it is.
fn settle(remote: &Remote, before: &Page) {
    let start = Instant::now();
    while start.elapsed() < LOAD_WAIT {
        std::thread::sleep(Duration::from_millis(50));
        let page = remote.page();
        if page.loading {
            continue;
        }
        let moved = page.loads > before.loads || page.url != before.url;
        // A page that did not start loading soon stays where it is.
        if moved || start.elapsed() > Duration::from_millis(600) {
            break;
        }
    }
}

/// Where the page is now, for the answer of an action. The page itself
/// says it, since a script may have changed the title a moment ago.
fn now_at(remote: &Remote) -> String {
    let asked = remote.call("Runtime.evaluate", json!({ "expression": "[location.href, document.title]", "returnByValue": true }), WAIT);
    let (url, title) = match asked.as_ref().ok().and_then(|r| r["result"]["value"].as_array().cloned()) {
        Some(v) => (
            v.first().and_then(Value::as_str).unwrap_or_default().to_string(),
            v.get(1).and_then(Value::as_str).unwrap_or_default().to_string(),
        ),
        None => {
            let page = remote.page();
            (page.url, page.title)
        }
    };
    let url = short(&url);
    if title.is_empty() { format!("The page is now {url}.") } else { format!("The page is now {url} ({title:?}).") }
}

/// A URL cut to 120 characters for an answer, long data URLs say nothing.
fn short(url: &str) -> String {
    if url.chars().count() > 120 { format!("{}…", url.chars().take(119).collect::<String>()) } else { url.to_string() }
}

impl AgentApp for BrowserApp {
    fn kind(&self) -> &'static str {
        "browser"
    }

    fn actions(&self) -> &'static [&'static str] {
        &["open", "click", "type", "scroll", "back", "forward", "reload"]
    }

    fn read(&mut self, core: &mut Core) -> Reply<AppText> {
        let remote = match self.remote(core) {
            Ok(r) => r,
            Err(e) => return Reply::err(e),
        };
        Reply::thread(move || {
            agent::wait_ready(&remote, WAIT)?;
            let page = agent::read(&remote, WAIT)?;
            let info = remote.page();
            let elements: Vec<Element> =
                page.elements.into_iter().map(|e| Element { r#ref: e.r#ref, role: e.role, name: e.name, value: e.value }).collect();
            let mut extra = Map::new();
            extra.insert("url".into(), json!(page.url));
            extra.insert("can_go_back".into(), json!(info.can_go_back));
            extra.insert("can_go_forward".into(), json!(info.can_go_forward));
            let summary = format!("{} with {} elements to act on. Click or type into them by ref.", page.url, elements.len());
            Ok(AppText { kind: "browser".into(), title: page.title, summary, extra, elements, content: page.text, ..Default::default() })
        })
    }

    fn act(&mut self, core: &mut Core, action: &str, args: &Map<String, Value>) -> Reply<Acted> {
        let handle = self.handle();
        let dir = core.agent_dir(&self.agent).map(|d| d.path.clone());
        // Checked before Chrome starts, so a wrong call costs nothing.
        let url = if action == "open" {
            let raw = match text_arg(args, "url", "open") {
                Ok(u) => u,
                Err(e) => return Reply::err(e),
            };
            match meerora_browser::url::for_agent(raw.trim(), dir.as_deref()) {
                Some(url) => Some(url),
                None => {
                    return Reply::err(format!(
                        "meerora does not open {:?} for you. Agents open http, https and data URLs and files inside their own directory.",
                        raw.trim()
                    ));
                }
            }
        } else {
            None
        };
        let remote = match self.remote(core) {
            Ok(r) => r,
            Err(e) => return Reply::err(e),
        };
        crate::ui::browser::agent_drives(core, &self.agent, self.n);
        match action {
            "open" => {
                let url = url.unwrap_or_default();
                Reply::thread(move || {
                    agent::open(&remote, &url, WAIT)?;
                    Ok(Acted::new(format!("Opened {} in {handle}. {}", short(&url), now_at(&remote)), url))
                })
            }
            "click" => {
                let target = match text_arg(args, "ref", "click") {
                    Ok(r) => r,
                    Err(e) => return Reply::err(e),
                };
                Reply::thread(move || {
                    agent::wait_ready(&remote, WAIT)?;
                    let before = remote.page();
                    let element = agent::click(&remote, target.trim(), WAIT)?;
                    settle(&remote, &before);
                    let name = if element.name.is_empty() { element.r#ref.clone() } else { element.name.clone() };
                    let text = format!("Clicked the {} {name:?} in {handle}. {}", element.role, now_at(&remote));
                    Ok(Acted::new(text, name))
                })
            }
            "type" => {
                let target = match text_arg(args, "ref", "type") {
                    Ok(r) => r,
                    Err(e) => return Reply::err(e),
                };
                let text = match text_arg(args, "text", "type") {
                    Ok(t) => t,
                    Err(e) => return Reply::err(e),
                };
                let submit = bool_arg(args, "submit");
                Reply::thread(move || {
                    agent::wait_ready(&remote, WAIT)?;
                    let before = remote.page();
                    let element = agent::type_into(&remote, target.trim(), &text, submit, WAIT)?;
                    if submit {
                        settle(&remote, &before);
                    }
                    let name = if element.name.is_empty() { element.r#ref.clone() } else { element.name.clone() };
                    let done = if submit { format!(" and pressed Enter. {}", now_at(&remote)) } else { ".".to_string() };
                    let answer = format!("Typed {} characters into the {} {name:?} in {handle}{done}", text.chars().count(), element.role);
                    // The log names the field, not what went into it, which may be private.
                    Ok(Acted::new(answer, if submit { format!("{name} (submitted)") } else { name }))
                })
            }
            "scroll" => {
                let dy = match number_arg(args, "dy", "scroll", None) {
                    Ok(d) => d as f32,
                    Err(e) => return Reply::err(e),
                };
                Reply::thread(move || {
                    agent::scroll(&remote, dy, WAIT)?;
                    let way = if dy < 0.0 { "up" } else { "down" };
                    Ok(Acted::new(format!("Scrolled {handle} {way} by {} pixels.", dy.abs().round()), format!("{dy}")))
                })
            }
            "back" | "forward" | "reload" => {
                let action = action.to_string();
                Reply::thread(move || {
                    let before = remote.page();
                    match action.as_str() {
                        "back" if !before.can_go_back => return Err(format!("{handle} has no page to go back to.")),
                        "forward" if !before.can_go_forward => return Err(format!("{handle} has no page to go forward to.")),
                        "back" => remote.back(),
                        "forward" => remote.forward(),
                        _ => remote.reload(),
                    }
                    settle(&remote, &before);
                    let done = match action.as_str() {
                        "back" => "Went back",
                        "forward" => "Went forward",
                        _ => "Reloaded the page",
                    };
                    Ok(Acted::new(format!("{done} in {handle}. {}", now_at(&remote)), remote.page().url))
                })
            }
            other => Reply::err(format!("A browser has no action {other:?}.")),
        }
    }

    fn screenshot(&mut self, core: &mut Core) -> Option<Reply<Vec<u8>>> {
        let remote = match self.remote(core) {
            Ok(r) => r,
            Err(e) => return Some(Reply::err(e)),
        };
        Some(Reply::thread(move || {
            agent::wait_ready(&remote, WAIT)?;
            agent::screenshot(&remote, WAIT)
        }))
    }

    fn taken_over(&self, core: &mut Core) -> bool {
        crate::ui::browser::taken_over(core, &self.agent, self.n)
    }
}
