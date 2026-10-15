//! The command box in the real UI on a headless GPU: opening, typing,
//! moving the selection, running a row and closing, with a data dir of its
//! own. Nothing here starts an app or reaches the daemon.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use egui::{Event, Key, Modifiers, PointerButton, Pos2, Rect, vec2};

use super::*;
use crate::assets::Assets;

fn screen() -> Rect {
    Rect::from_min_size(Pos2::ZERO, vec2(1440.0, 900.0))
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap().flatten() {
        let target = to.join(entry.file_name());
        if entry.path().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

const ECHO: &str = r#"export default command({
  keyword: "echo",
  title: "Echo",
  run(q) {
    if (q === "boom") throw new Error("no echo for boom");
    return [{ title: "said " + q, action: copy(q), alt: notify("heard " + q) }];
  },
});"#;

struct Harness {
    gpu: meerora_scene::Gpu,
    ctx: egui::Context,
    core: Core,
    data: PathBuf,
    time: f64,
    copied: Vec<String>,
}

impl Harness {
    fn new(name: &str) -> Option<Harness> {
        let gpu = match meerora_scene::Gpu::headless() {
            Ok(gpu) => gpu,
            Err(e) => {
                eprintln!("skipped: no headless GPU ({e:#})");
                return None;
            }
        };
        let assets = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets");
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().subsec_nanos();
        let data = std::env::temp_dir().join(format!("meerora-commands-{name}-{}-{nanos}", std::process::id()));
        copy_dir(&assets.join("spaces"), &data.join("spaces"));
        // Only the chat in their views, so no shell and no Chrome start.
        for agent in ["wren", "claude"] {
            copy_dir(&assets.join("agents").join(agent), &data.join("agents").join(agent));
            let toml = data.join("agents").join(agent).join("agent.toml");
            let text = std::fs::read_to_string(&toml).unwrap();
            let home = text.lines().find(|l| l.starts_with("home = ")).unwrap().to_string();
            std::fs::write(&toml, text.replacen(&home, &format!("{home}\napps = [\"chat\"]"), 1)).unwrap();
        }
        std::fs::create_dir_all(data.join("commands")).unwrap();
        std::fs::write(data.join("commands/echo.js"), ECHO).unwrap();
        std::fs::write(data.join("commands.toml"), "[[command]]\nkeyword = \"shrug\"\ntitle = \"Copy a shrug\"\ncopy = \"meh\"\n").unwrap();
        let mut core = Core::new(&gpu.device, meerora_scene::IMAGE_FORMAT, Assets::without_watcher(data.clone()), None, Arc::new(|| {}));
        core.load(&gpu.device, &gpu.queue);
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        let mut h = Harness { gpu, ctx, core, data, time: 0.0, copied: Vec::new() };
        h.frame(Vec::new());
        Some(h)
    }

    fn frame(&mut self, events: Vec<Event>) {
        self.time += 1.0 / 30.0;
        self.core.step(&self.gpu.device, &self.gpu.queue, 1.0 / 30.0);
        let raw = egui::RawInput { screen_rect: Some(screen()), time: Some(self.time), events, ..Default::default() };
        let core = &mut self.core;
        let out = self.ctx.run_ui(raw, |ui| core.build_ui(ui));
        for command in &out.platform_output.commands {
            if let egui::OutputCommand::CopyText(text) = command {
                self.copied.push(text.clone());
            }
        }
        self.core.apply_textures(&self.gpu.device, &self.gpu.queue, out.textures_delta);
    }

    fn press(&mut self, key: Key, modifiers: Modifiers) {
        let event = |pressed| Event::Key { key, physical_key: Some(key), pressed, repeat: false, modifiers };
        self.frame(vec![event(true), event(false)]);
    }

    fn type_text(&mut self, text: &str) {
        self.frame(vec![Event::Text(text.into())]);
        self.frame(Vec::new());
    }

    /// Runs frames until the box loaded its files and the scripts answered.
    fn settle(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            self.frame(Vec::new());
            let b = &self.core.ui.commands;
            if b.loaded.is_some() && b.loading.is_none() && b.scripts.as_ref().is_none_or(|s| s.settled()) {
                self.frame(Vec::new());
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("the command box did not settle");
    }

    fn titles(&self) -> Vec<String> {
        self.core.ui.commands.rows.iter().map(|r| r.title.clone()).collect()
    }

    fn selected(&self) -> String {
        let b = &self.core.ui.commands;
        b.rows.get(b.selected).map(|r| r.title.clone()).unwrap_or_default()
    }

    fn open(&mut self) {
        toggle(&mut self.core);
        self.frame(Vec::new());
        self.settle();
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        // The data dir is this test's own copy.
        let _ = std::fs::remove_dir_all(&self.data);
    }
}

#[test]
fn keys_open_navigate_run_and_close() {
    let Some(mut h) = Harness::new("keys") else { return };
    assert!(!h.core.ui.commands.open);
    h.open();
    assert!(h.core.ui.commands.open);
    assert!(h.ctx.memory(|m| m.has_focus(input_id())), "the field has the keyboard");

    // Math shows live and Enter copies it, then the box closes.
    h.type_text("2+3*4");
    assert_eq!(h.titles()[0], "= 14");
    h.press(Key::Enter, Modifiers::NONE);
    assert_eq!(h.copied, ["14"]);
    assert!(!h.core.ui.commands.open, "Enter closed the box");
    assert!(h.core.ui.commands.query.is_empty(), "a closed box forgets its query");

    // Up and Down move and wrap, Enter opens the chosen agent's view.
    h.open();
    h.type_text("wren");
    assert_eq!(h.selected(), "Wren");
    let count = h.core.ui.commands.rows.len();
    h.press(Key::ArrowUp, Modifiers::NONE);
    assert_eq!(h.core.ui.commands.selected, count - 1, "Up from the first row wraps to the last");
    h.press(Key::ArrowDown, Modifiers::NONE);
    assert_eq!(h.selected(), "Wren");
    h.press(Key::Enter, Modifiers::NONE);
    assert_eq!(h.core.ui.view, View::Agent("wren".into()));
    assert!(!h.core.ui.commands.open);

    // Esc closes only the box, the agent view stays.
    h.open();
    h.type_text("term");
    h.press(Key::Escape, Modifiers::NONE);
    assert!(!h.core.ui.commands.open);
    assert_eq!(h.core.ui.view, View::Agent("wren".into()), "Esc did not reach the agent view");

    // A built-in app opens in the agent's view.
    h.open();
    h.type_text("Activity");
    assert_eq!(h.selected(), "Activity");
    h.press(Key::Enter, Modifiers::NONE);
    h.frame(Vec::new());
    assert_eq!(h.core.ui.tab, AppTab::Activity);

    // Toggling closes it again, and a click outside the box too.
    h.open();
    toggle(&mut h.core);
    assert!(!h.core.ui.commands.open);
    h.open();
    let outside = Pos2::new(40.0, 860.0);
    h.frame(vec![Event::PointerMoved(outside)]);
    h.frame(vec![Event::PointerButton { pos: outside, button: PointerButton::Primary, pressed: true, modifiers: Modifiers::NONE }]);
    h.frame(vec![Event::PointerButton { pos: outside, button: PointerButton::Primary, pressed: false, modifiers: Modifiers::NONE }]);
    assert!(!h.core.ui.commands.open, "a click outside closes the box");
}

#[test]
fn tab_completes_and_scripts_answer_in_the_box() {
    let Some(mut h) = Harness::new("scripts") else { return };
    h.open();
    h.type_text("ech");
    assert_eq!(h.selected(), "Echo", "{:?}", h.titles());
    h.press(Key::Tab, Modifiers::NONE);
    assert_eq!(h.core.ui.commands.query, "echo ");
    assert!(h.ctx.memory(|m| m.has_focus(input_id())), "Tab did not move the focus away");
    h.type_text("hello");
    h.settle();
    assert_eq!(h.titles(), ["said hello"]);
    assert!(h.core.ui.commands.rows[0].subtitle.contains("Enter copies \"hello\""));
    // Shift+Enter runs the item's alternative.
    h.press(Key::Enter, Modifiers::SHIFT);
    assert!(!h.core.ui.commands.open);
    assert!(h.core.ui.toasts.iter().any(|t| t.text == "heard hello"), "the note shows");
    assert!(h.copied.is_empty());

    // A script error is a row.
    h.open();
    h.type_text("echo boom");
    h.settle();
    let row = &h.core.ui.commands.rows[0];
    assert!(row.error && row.title.contains("no echo for boom"), "{row:?}");

    // A keyword of commands.toml without {query} runs at once.
    h.press(Key::Escape, Modifiers::NONE);
    h.open();
    h.type_text("shrug");
    assert_eq!(h.selected(), "Copy a shrug");
    h.press(Key::Enter, Modifiers::NONE);
    assert_eq!(h.copied, ["meh"]);
    // What ran comes first in the empty box next time.
    h.open();
    assert_eq!(h.selected(), "Copy a shrug");
    let history = std::fs::read_to_string(h.data.join("commands-recent.json"));
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut history = history.unwrap_or_default();
    while !history.contains("cmd:shrug") && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
        history = std::fs::read_to_string(h.data.join("commands-recent.json")).unwrap_or_default();
    }
    assert!(history.contains("cmd:shrug"), "{history}");
}

#[test]
fn ctrl_tab_picks_where_apps_start() {
    let Some(mut h) = Harness::new("target") else { return };
    h.core.ui.view = View::Agent("wren".into());
    h.frame(Vec::new());
    h.core.ui.view = View::Space;
    h.open();
    h.type_text("Terminal");
    let row = h.core.ui.commands.rows.iter().find(|r| r.title == "Terminal").unwrap();
    assert!(row.subtitle.contains("Wren's view"), "the agent viewed last: {}", row.subtitle);
    h.press(Key::Tab, Modifiers::CTRL);
    let row = h.core.ui.commands.rows.iter().find(|r| r.title == "Terminal").unwrap();
    assert!(row.subtitle.contains("Claude's view"), "Ctrl+Tab moved on: {}", row.subtitle);
    h.press(Key::Tab, Modifiers::CTRL | Modifiers::SHIFT);
    let row = h.core.ui.commands.rows.iter().find(|r| r.title == "Terminal").unwrap();
    assert!(row.subtitle.contains("Wren's view"), "Ctrl+Shift+Tab went back: {}", row.subtitle);
}

#[test]
fn examples_appear_on_first_open_and_answers_update_the_row() {
    let Some(mut h) = Harness::new("examples") else { return };
    std::fs::remove_file(h.data.join("commands.toml")).unwrap();
    std::fs::remove_dir_all(h.data.join("commands")).unwrap();
    h.open();
    assert!(h.data.join("commands.toml").is_file(), "the example was written on first open");
    assert!(h.data.join("commands/units.js").is_file() && h.data.join("commands/github.js").is_file());
    h.type_text("temp 100");
    h.settle();
    assert_eq!(h.titles()[0], "100 °C = 212 °F");

    // An answer of the daemon for `??` fills the row, and only its own.
    h.core.ui.commands.answer = Some(Answer {
        request: "r1".into(),
        agent: "claude".into(),
        question: "why".into(),
        text: String::new(),
        done: false,
        error: None,
    });
    h.core.ui.daemon_connected = true;
    for (request, text) in [("other", "not mine"), ("r1", "Because.")] {
        let event = meerora_protocol::Event::Answer { request: request.into(), text: text.into(), done: request == "r1", error: None };
        assert!(on_event(&mut h.core, &event));
    }
    let answer = h.core.ui.commands.answer.clone().unwrap();
    assert_eq!((answer.text.as_str(), answer.done), ("Because.", true));
}
