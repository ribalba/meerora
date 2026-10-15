//! The editor, Files and the Viewer as agents use them (D30). The editor
//! reads its open files and the active file around the cursor, Files the
//! folder it shows and the Viewer the file it shows. Their action is
//! `open {path}`, plus `goto {line}` in the editor. Agents change files by
//! writing them, the editor shows and reviews those changes.
//!
//! Every path stays inside the agent's directory (`super::inside`), with
//! symbolic links resolved.

use std::path::{Path, PathBuf};

use meerora_protocol::App;
use serde_json::{Map, Value, json};

use super::{Acted, AgentApp, AppText, Element, Reply, inside, number_arg, text_arg};
use crate::engine::Core;
use crate::ui::AppTab;
use crate::ui::apps::viewer::{self, Kind, Route};

/// Lines of the active file that `app_read` returns around the cursor.
const AROUND: usize = 120;
/// The most bytes of a text file the Viewer returns.
const TEXT_BYTES: u64 = 200_000;

fn agent_dir(core: &Core, agent: &str) -> Result<PathBuf, String> {
    core.agent_dir(agent).map(|d| d.path.clone()).ok_or_else(|| "This agent has no directory.".into())
}

fn shown(rel: &Path) -> String {
    if rel.as_os_str().is_empty() { ".".into() } else { rel.display().to_string() }
}

pub struct EditorApp {
    pub agent: String,
    pub n: u32,
}

impl AgentApp for EditorApp {
    fn kind(&self) -> &'static str {
        "editor"
    }

    fn actions(&self) -> &'static [&'static str] {
        &["open", "goto"]
    }

    fn read(&mut self, core: &mut Core) -> Reply<AppText> {
        let Some(view) = crate::ui::editor::agent_view(core, &self.agent, self.n) else {
            return Reply::err("This agent has no directory.");
        };
        let mut extra = Map::new();
        extra.insert("open_files".into(), json!(view.open.iter().map(|(rel, _)| rel.display().to_string()).collect::<Vec<_>>()));
        let unsaved: Vec<String> = view.open.iter().filter(|(_, d)| *d).map(|(rel, _)| rel.display().to_string()).collect();
        extra.insert("unsaved".into(), json!(unsaved));
        let Some(active) = view.active else {
            let summary = "No file is open. Open one with app_act open {path}.".to_string();
            return Reply::ok(AppText { kind: "editor".into(), title: format!("Editor {}", self.n), summary, extra, ..Default::default() });
        };
        let lines: Vec<&str> = active.text.lines().collect();
        let line = active.cursor.map_or(1, |(l, _)| l).clamp(1, lines.len().max(1));
        let first = line.saturating_sub(AROUND / 2).max(1);
        let last = (first + AROUND - 1).min(lines.len());
        // Numbered lines, so goto and the agent's own edits can refer to them.
        let content: String = (first..=last).map(|i| format!("{i:>5}  {}", lines[i - 1])).collect::<Vec<_>>().join("\n");
        extra.insert("file".into(), json!(active.rel.display().to_string()));
        extra.insert("lines".into(), json!(lines.len()));
        extra.insert("shown_lines".into(), json!(format!("{first}-{last}")));
        if let Some((l, c)) = active.cursor {
            extra.insert("cursor".into(), json!({ "line": l, "column": c }));
        }
        if let Some(note) = &active.note {
            extra.insert("note".into(), json!(note));
        }
        let state = if active.unsaved { "with unsaved edits of the user" } else { "saved" };
        let summary = format!("{} ({} lines, {state}), lines {first} to {last} around the cursor.", active.rel.display(), lines.len());
        Reply::ok(AppText { kind: "editor".into(), title: active.rel.display().to_string(), summary, extra, content, ..Default::default() })
    }

    fn act(&mut self, core: &mut Core, action: &str, args: &Map<String, Value>) -> Reply<Acted> {
        let handle = format!("editor:{}", self.n);
        match action {
            "open" => {
                let dir = match agent_dir(core, &self.agent) {
                    Ok(d) => d,
                    Err(e) => return Reply::err(e),
                };
                let rel = match text_arg(args, "path", "open").and_then(|p| inside(&dir, &p)) {
                    Ok(r) => r,
                    Err(e) => return Reply::err(e),
                };
                if dir.join(&rel).is_dir() {
                    return Reply::err(format!("{} is a folder. The editor opens files, Files shows folders.", shown(&rel)));
                }
                crate::ui::editor::agent_open(core, &self.agent, self.n, &rel);
                let text = format!("Opened {} in {handle}. Read it with app_read.", rel.display());
                Reply::ok(Acted::new(text, rel.display().to_string()))
            }
            "goto" => {
                let line = match number_arg(args, "line", "goto", None) {
                    Ok(l) if l >= 1.0 && l.fract() == 0.0 => l as usize,
                    Ok(l) => return Reply::err(format!("line is a whole number from 1, not {l}.")),
                    Err(e) => return Reply::err(e),
                };
                match crate::ui::editor::agent_goto(core, &self.agent, self.n, line) {
                    Ok(rel) => Reply::ok(Acted::new(format!("{handle} shows line {line} of {} now.", rel.display()), format!("{line}"))),
                    Err(e) => Reply::err(e),
                }
            }
            other => Reply::err(format!("An editor has no action {other:?}.")),
        }
    }

    fn taken_over(&self, core: &mut Core) -> bool {
        crate::ui::editor::taken_over(core, &self.agent, self.n)
    }
}

pub struct FilesApp {
    pub agent: String,
    pub n: u32,
}

impl AgentApp for FilesApp {
    fn kind(&self) -> &'static str {
        "files"
    }

    fn actions(&self) -> &'static [&'static str] {
        &["open"]
    }

    fn read(&mut self, core: &mut Core) -> Reply<AppText> {
        let Some((cwd, listing)) = crate::ui::apps::files::agent_listing(core, &self.agent, self.n) else {
            return Reply::err("This agent has no directory.");
        };
        let items = match listing {
            Ok(items) => items,
            Err(e) => return Reply::err(e),
        };
        let mut elements = Vec::new();
        let mut lines = Vec::new();
        for (i, item) in items.iter().enumerate() {
            let path = cwd.join(&item.name);
            let (role, size) =
                if item.dir { ("folder", format!("{} entries", item.size)) } else { ("file", format!("{} bytes", item.size)) };
            lines.push(format!("{}{}  ({size})", item.name, if item.dir { "/" } else { "" }));
            elements.push(Element { r#ref: format!("f{}", i + 1), role: role.into(), name: path.display().to_string(), value: None });
        }
        let mut extra = Map::new();
        extra.insert("folder".into(), json!(shown(&cwd)));
        let summary = format!(
            "The folder {} of your directory with {} entries. open {{path}} with an entry's name opens it, folders here, files in the \
             editor or the viewer.",
            shown(&cwd),
            items.len()
        );
        Reply::ok(AppText {
            kind: "files".into(),
            title: format!("Files: {}", shown(&cwd)),
            summary,
            extra,
            elements,
            content: lines.join("\n"),
            ..Default::default()
        })
    }

    fn act(&mut self, core: &mut Core, action: &str, args: &Map<String, Value>) -> Reply<Acted> {
        if action != "open" {
            return Reply::err(format!("Files has no action {action:?}."));
        }
        let dir = match agent_dir(core, &self.agent) {
            Ok(d) => d,
            Err(e) => return Reply::err(e),
        };
        let rel = match text_arg(args, "path", "open").and_then(|p| inside(&dir, &p)) {
            Ok(r) => r,
            Err(e) => return Reply::err(e),
        };
        let handle = format!("files:{}", self.n);
        if dir.join(&rel).is_dir() {
            crate::ui::apps::files::agent_go(core, &self.agent, self.n, &rel);
            return Reply::ok(Acted::new(format!("{handle} shows the folder {} now.", shown(&rel)), shown(&rel)));
        }
        match open_file(core, &self.agent, &dir, &rel) {
            Ok(tab) => Reply::ok(Acted::new(format!("Opened {} in {}.", rel.display(), tab.handle()), rel.display().to_string())),
            Err(e) => Reply::err(e),
        }
    }
}

/// Opens a file of the agent like a double click in Files: text and code in
/// the editor, results in the Viewer. Returns where it opened.
pub fn open_file(core: &mut Core, agent: &str, dir: &Path, rel: &Path) -> Result<AppTab, String> {
    let tab = match viewer::route(&dir.join(rel)) {
        Route::Editor => {
            let tab = crate::ui::agent::target(core, agent, App::Editor);
            crate::ui::editor::agent_open(core, agent, tab.instance(), rel);
            tab
        }
        Route::Viewer(_) => {
            let tab = crate::ui::agent::target(core, agent, App::Viewer);
            viewer::agent_show(core, agent, tab.instance(), rel);
            tab
        }
    };
    super::open_tile(core, agent, tab);
    Ok(tab)
}

pub struct ViewerApp {
    pub agent: String,
    pub n: u32,
}

impl AgentApp for ViewerApp {
    fn kind(&self) -> &'static str {
        "viewer"
    }

    fn actions(&self) -> &'static [&'static str] {
        &["open"]
    }

    fn read(&mut self, core: &mut Core) -> Reply<AppText> {
        let dir = match agent_dir(core, &self.agent) {
            Ok(d) => d,
            Err(e) => return Reply::err(e),
        };
        let Some(rel) = viewer::agent_shown(core, &self.agent, self.n) else {
            let summary = "The viewer shows no file. Show one with app_act open {path}.".to_string();
            return Reply::ok(AppText { kind: "viewer".into(), title: format!("Viewer {}", self.n), summary, ..Default::default() });
        };
        let path = dir.join(&rel);
        let kind = viewer::kind_of(&path);
        let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let mut extra = Map::new();
        extra.insert("file".into(), json!(rel.display().to_string()));
        extra.insert("bytes".into(), json!(size));
        let readable = matches!(kind, Kind::Markdown | Kind::Text | Kind::Csv | Kind::Html);
        let (summary, content) = if readable && size <= TEXT_BYTES {
            let text = std::fs::read(&path).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default();
            (format!("{} as the viewer shows it.", rel.display()), text)
        } else if readable {
            (format!("{} is too large to read here, {size} bytes. Read the file yourself.", rel.display()), String::new())
        } else {
            (format!("{} is a {kind:?} file. The viewer shows it as a picture or a document, not as text.", rel.display()), String::new())
        };
        Reply::ok(AppText { kind: "viewer".into(), title: rel.display().to_string(), summary, extra, content, ..Default::default() })
    }

    fn act(&mut self, core: &mut Core, action: &str, args: &Map<String, Value>) -> Reply<Acted> {
        if action != "open" {
            return Reply::err(format!("The viewer has no action {action:?}."));
        }
        let dir = match agent_dir(core, &self.agent) {
            Ok(d) => d,
            Err(e) => return Reply::err(e),
        };
        let rel = match text_arg(args, "path", "open").and_then(|p| inside(&dir, &p)) {
            Ok(r) => r,
            Err(e) => return Reply::err(e),
        };
        if dir.join(&rel).is_dir() {
            return Reply::err(format!("{} is a folder. The viewer shows files.", shown(&rel)));
        }
        viewer::agent_show(core, &self.agent, self.n, &rel);
        Reply::ok(Acted::new(format!("viewer:{} shows {} now.", self.n, rel.display()), rel.display().to_string()))
    }
}
