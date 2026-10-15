//! `app_open` (D30): a new instance of a built-in app in the agent's view,
//! or a real app started in the agent's sandbox on its desktop.
//!
//! Built-in apps open through the same layout as the launcher's "New", next
//! to their siblings, without taking the user's selected tab. A browser may
//! get `url`, which passes the agent rules of `url::for_agent`, and the
//! editor, Files and the Viewer may get `path` inside the agent's directory.
//!
//! A real app is found by its desktop id (`org.gnome.Calculator.desktop`,
//! with or without `.desktop`), by its name in the launcher or as a program
//! on PATH. It starts through `Compositor::launch_sandboxed`, with the
//! network only when the agent holds `web` (D29). The answer waits up to
//! [`WINDOW_WAIT`] for its window and returns its handle.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use meerora_compositor::SandboxOptions;
use meerora_protocol::{App, BUILT_IN_APPS};
use serde_json::{Map, Value};

use super::{Acted, Reply, inside};
use crate::engine::Core;
use crate::ui::AppTab;

/// How long `app_open` waits for the window of a real app.
const WINDOW_WAIT: Duration = Duration::from_secs(12);

/// Opens `app` for `agent` and returns the new instance.
pub fn open(core: &mut Core, agent: &str, app: &str, args: &Map<String, Value>, granted: &[String]) -> Reply<(AppTab, Acted)> {
    let name = app.trim();
    let kind = name.to_lowercase();
    if BUILT_IN_APPS.contains(&kind.as_str()) {
        return built_in(core, agent, &kind, args);
    }
    real_app(core, agent, name, args, granted)
}

fn built_in(core: &mut Core, agent: &str, kind: &str, args: &Map<String, Value>) -> Reply<(AppTab, Acted)> {
    let Some(first) = AppTab::parse_handle(kind) else {
        return Reply::err(format!("The {kind} app is not part of this meerora yet."));
    };
    let Some(app) = first.app() else { return Reply::err(format!("{kind} is not a built-in app.")) };
    let open = super::open_tabs(core, agent);
    let tab = if crate::ui::multi(app) { crate::ui::agent::next_instance(core, agent, app) } else { first };
    if !crate::ui::multi(app) && open.contains(&tab) {
        let handle = tab.handle();
        return Reply::ok((tab, Acted::new(format!("{handle} is open already, it runs once."), handle)));
    }
    let dir = core.agent_dir(agent).map(|d| d.path.clone());
    let path = match (args.get("path").and_then(Value::as_str), &dir) {
        (Some(p), Some(dir)) => match inside(dir, p) {
            Ok(rel) => Some(rel),
            Err(e) => return Reply::err(e),
        },
        _ => None,
    };
    let url = match args.get("url").and_then(Value::as_str) {
        Some(raw) if app == App::Browser => match meerora_browser::url::for_agent(raw.trim(), dir.as_deref()) {
            Some(url) => Some(url),
            None => return Reply::err(format!("meerora does not open {:?} for you. Agents open http, https and data URLs.", raw.trim())),
        },
        _ => None,
    };
    if let (Some(rel), Some(dir)) = (&path, &dir) {
        let is_dir = dir.join(rel).is_dir();
        match app {
            App::Editor | App::Viewer if is_dir => return Reply::err(format!("{} is a folder.", rel.display())),
            App::Files if !is_dir => return Reply::err(format!("{} is a file. Files opens folders.", rel.display())),
            _ => {}
        }
    }
    super::open_tile(core, agent, tab);
    let handle = tab.handle();
    let mut text = format!("Opened {handle}.");
    let mut summary = handle.clone();
    match (tab, &path, &url) {
        (AppTab::Terminal(n), _, _) => {
            if core.terminal(agent, n).is_none() {
                return Reply::err(format!("{handle} opened, but its shell could not start."));
            }
            text.push_str(" Its shell runs in your directory.");
        }
        (AppTab::Browser(n), _, Some(url)) => {
            crate::ui::browser::navigate(core, agent, n, url);
            if let Err(e) = crate::ui::browser::agent_page(core, agent, n) {
                return Reply::err(e);
            }
            crate::ui::browser::agent_drives(core, agent, n);
            text = format!("Opened {handle} with {url}.");
            summary = url.clone();
        }
        (AppTab::Editor(n), Some(rel), _) => {
            crate::ui::editor::agent_open(core, agent, n, rel);
            text = format!("Opened {handle} with {}.", rel.display());
        }
        (AppTab::Files(n), Some(rel), _) => {
            crate::ui::apps::files::agent_go(core, agent, n, rel);
            text = format!("Opened {handle} in the folder {}.", rel.display());
        }
        (AppTab::Viewer(n), Some(rel), _) => {
            crate::ui::apps::viewer::agent_show(core, agent, n, rel);
            text = format!("Opened {handle} with {}.", rel.display());
        }
        _ => {}
    }
    Reply::ok((tab, Acted::new(text, summary)))
}

/// The program and arguments of a real app, and the name to show.
fn find(core: &mut Core, name: &str) -> Result<(String, String, Vec<String>), String> {
    let wanted = name.to_lowercase();
    let bare = wanted.strip_suffix(".desktop").unwrap_or(&wanted).to_string();
    let list = core.desktops.app_list();
    let found = list
        .apps
        .iter()
        .find(|a| a.id.to_lowercase().strip_suffix(".desktop").unwrap_or(&a.id.to_lowercase()) == bare)
        .or_else(|| list.apps.iter().find(|a| a.name.to_lowercase() == wanted));
    if let Some(app) = found {
        let (program, args) = app.exec.split_first().ok_or_else(|| format!("{} has no command to start.", app.name))?;
        return Ok((app.name.clone(), program.clone(), args.to_vec()));
    }
    let program = if name.contains('/') {
        let path = PathBuf::from(name);
        (path.is_absolute() && executable(&path)).then_some(path)
    } else {
        std::env::var_os("PATH").and_then(|p| std::env::split_paths(&p).map(|d| d.join(name)).find(|p| executable(p)))
    };
    match program {
        Some(p) => Ok((name.to_string(), p.display().to_string(), Vec::new())),
        None => Err(format!(
            "{name:?} is neither a built-in app ({}) nor an installed app or program.",
            BUILT_IN_APPS.iter().filter(|k| AppTab::parse_handle(k).is_some()).copied().collect::<Vec<_>>().join(", ")
        )),
    }
}

fn executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

fn real_app(core: &mut Core, agent: &str, name: &str, args: &Map<String, Value>, granted: &[String]) -> Reply<(AppTab, Acted)> {
    let (shown, program, mut argv) = match find(core, name) {
        Ok(found) => found,
        Err(e) => return Reply::err(e),
    };
    match args.get("args") {
        None | Some(Value::Null) => {}
        Some(Value::Array(extra)) => {
            for a in extra {
                match a.as_str() {
                    Some(s) => argv.push(s.to_string()),
                    None => return Reply::err(format!("args of app_open are strings, not {a}.")),
                }
            }
        }
        Some(other) => return Reply::err(format!("args of app_open is a list of strings, not {other}.")),
    }
    let Some(dir) = core.agent_dir(agent).map(|d| d.path.clone()) else { return Reply::err("This agent has no directory.") };
    let dir = dir.canonicalize().unwrap_or(dir);
    let before: HashSet<u32> = core.desktops.windows().iter().filter(|w| w.desktop == agent).map(|w| w.id).collect();
    let options = SandboxOptions { network: granted.iter().any(|g| g == "web"), ..Default::default() };
    if let Err(e) = core.desktops.launch_sandboxed(agent, &dir, &shown, &program, &argv, &options) {
        return Reply::err(e);
    }
    let network = if options.network { "with" } else { "without" };
    let (agent, started) = (agent.to_string(), Instant::now());
    Reply::Later(Box::new(move |core: &mut Core| {
        core.desktops.dispatch();
        let new = core.desktops.windows().into_iter().find(|w| w.desktop == agent && w.sandboxed && !before.contains(&w.id));
        match new {
            Some(w) => {
                let tab = AppTab::Window(w.id);
                let text = format!("Started {shown} in your sandbox, {network} network. Its window is {}.", tab.handle());
                Some(Ok((tab, Acted::new(text, shown.clone()))))
            }
            None if started.elapsed() > WINDOW_WAIT => Some(Err(format!(
                "Started {shown} in your sandbox, but it showed no window within {} seconds. apps_list shows it once it does.",
                WINDOW_WAIT.as_secs()
            ))),
            None => None,
        }
    }))
}
