//! The app side of D30 with real instances: the real app on a headless GPU
//! with Claude's view open, real shells, headless Chrome and weston-terminal
//! on the nested compositor. Requests go in as `Event::AppRequest` and the
//! answers come out of the outbox, as the daemon would see them.
//!
//! With `MEERORA_APPS_PNG=<dir>` the tests also write snapshots of the
//! terminal and the browser tile after the agent acted, for checking by eye.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use egui::{Pos2, Rect, vec2};
use meerora_protocol::{AppCall, AppResult, Event};
use meerora_term::Spawn;
use serde_json::{Value, json};

use super::*;
use crate::assets::Assets;
use crate::ui::{AppTab, View};

const AGENT: &str = "claude";

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

fn gpu() -> Option<&'static meerora_scene::Gpu> {
    static GPU: OnceLock<Option<meerora_scene::Gpu>> = OnceLock::new();
    GPU.get_or_init(|| match meerora_scene::Gpu::headless() {
        Ok(gpu) => Some(gpu),
        Err(e) => {
            eprintln!("skipped: no headless GPU ({e:#})");
            None
        }
    })
    .as_ref()
}

fn installed(program: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(program).is_file()))
}

/// The real app on a headless GPU with Claude's view open, in its own data
/// directory. Shells run `/bin/sh` with their own HOME.
struct Harness {
    gpu: &'static meerora_scene::Gpu,
    ctx: egui::Context,
    core: Core,
    data: PathBuf,
    time: f64,
    seq: u32,
}

impl Harness {
    fn new(name: &str, apps: &str) -> Option<Harness> {
        Self::new_with(name, apps, |_| {})
    }

    /// Like [`Harness::new`], with `setup` run on the agent directory before the view first shows.
    fn new_with(name: &str, apps: &str, setup: impl FnOnce(&Path)) -> Option<Harness> {
        let gpu = gpu()?;
        let assets = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets");
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().subsec_nanos();
        let data = std::env::temp_dir().join(format!("meerora-agent-apps-{name}-{}-{nanos}", std::process::id()));
        copy_dir(&assets.join("spaces"), &data.join("spaces"));
        copy_dir(&assets.join("agents/claude"), &data.join("agents/claude"));
        let toml = data.join("agents/claude/agent.toml");
        let text = std::fs::read_to_string(&toml).unwrap().replace("home = \"bookshelf\"", &format!("home = \"bookshelf\"\napps = {apps}"));
        std::fs::write(&toml, text).unwrap();
        let home = data.join("home");
        std::fs::create_dir_all(&home).unwrap();
        setup(&data.join("agents/claude"));

        let mut core = Core::new(&gpu.device, meerora_scene::IMAGE_FORMAT, Assets::without_watcher(data.clone()), None, Arc::new(|| {}));
        core.load(&gpu.device, &gpu.queue);
        let home_env = home.display().to_string();
        core.terminal_spawn = Spawn {
            program: Some("/bin/sh".into()),
            env: ["HOME", "XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_CACHE_HOME", "XDG_STATE_HOME"]
                .into_iter()
                .map(|k| (k.to_string(), home_env.clone()))
                .chain([("HISTFILE".to_string(), "/dev/null".to_string()), ("PS1".to_string(), "$ ".to_string())])
                .collect(),
            ..Default::default()
        };
        core.ui.view = View::Agent(AGENT.into());
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        let mut h = Harness { gpu, ctx, core, data, time: 0.0, seq: 0 };
        h.frame();
        Some(h)
    }

    fn dir(&self) -> PathBuf {
        self.core.agent_dir(AGENT).unwrap().path.clone()
    }

    fn frame(&mut self) {
        self.time += 1.0 / 30.0;
        self.core.step(&self.gpu.device, &self.gpu.queue, 1.0 / 30.0);
        let raw = egui::RawInput { screen_rect: Some(screen()), time: Some(self.time), ..Default::default() };
        let core = &mut self.core;
        let out = self.ctx.run_ui(raw, |ui| core.build_ui(ui));
        self.core.apply_textures(&self.gpu.device, &self.gpu.queue, out.textures_delta);
    }

    /// Sends `request` as the daemon would and runs frames until the answer is out.
    fn call(&mut self, request: AppCall) -> AppResult {
        self.seq += 1;
        let id = format!("app-{}", self.seq);
        let granted = ["apps.read", "apps.control", "apps.input", "apps.launch"].map(String::from).to_vec();
        self.core.apply(&Event::AppRequest { id: id.clone(), agent: AGENT.into(), request: request.clone(), granted });
        let deadline = Instant::now() + Duration::from_secs(40);
        loop {
            self.frame();
            if let Some(i) = self.core.agent_apps.outbox.iter().position(|(got, _)| *got == id) {
                let result = self.core.agent_apps.outbox.remove(i).1;
                // The daemon logs a successful act for every client, its summary cut to 80 characters (app_tools.rs).
                if let Some(log) = result.log.as_ref().filter(|_| result.ok) {
                    let cut = log.summary.chars().count() > 80;
                    let summary =
                        if cut { format!("{}…", log.summary.chars().take(79).collect::<String>()) } else { log.summary.clone() };
                    let target = cut.then(|| log.summary.clone());
                    self.core.apply(&Event::Action { agent: AGENT.into(), tool: log.tool.clone(), summary, target });
                }
                return result;
            }
            assert!(Instant::now() < deadline, "no answer to {request:?}");
            std::thread::sleep(Duration::from_millis(15));
        }
    }

    fn act(&mut self, app: &str, action: &str, args: Value) -> AppResult {
        self.call(AppCall::Act { app: app.into(), action: action.into(), args })
    }

    fn read(&mut self, app: &str, query: Option<&str>) -> Value {
        let result = self.call(AppCall::Read { app: app.into(), query: query.map(String::from) });
        assert!(result.ok, "read {app}: {}", result.text);
        serde_json::from_str(&result.text).unwrap_or_else(|e| panic!("{e}: {}", result.text))
    }

    fn until(&mut self, timeout: Duration, mut done: impl FnMut(&mut Harness) -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            self.frame();
            if done(self) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    /// Writes the current UI to `<MEERORA_APPS_PNG>/<name>.png` when the variable is set.
    fn snapshot(&mut self, name: &str) {
        let Some(dir) = std::env::var_os("MEERORA_APPS_PNG") else { return };
        let path = PathBuf::from(dir).join(format!("{name}.png"));
        let (w, h) = (1440u32, 900u32);
        let (device, queue) = (&self.gpu.device, &self.gpu.queue);
        let format = meerora_scene::IMAGE_FORMAT;
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("agent apps snapshot"),
            size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[format.remove_srgb_suffix()],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let ui_view = texture.create_view(&wgpu::TextureViewDescriptor { format: Some(format.remove_srgb_suffix()), ..Default::default() });
        let mut output = None;
        for _ in 0..3 {
            self.core.step(device, queue, 1.0 / 30.0);
            let raw = egui::RawInput { screen_rect: Some(screen()), ..Default::default() };
            let core = &mut self.core;
            let out = self.ctx.run_ui(raw, |ui| core.build_ui(ui));
            if let Some(prev) = output.replace(out) {
                let prev: egui::FullOutput = prev;
                self.core.apply_textures(device, queue, prev.textures_delta);
            }
        }
        let output = output.unwrap();
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("agent apps snapshot") });
        self.core.paint(device, queue, &mut encoder, &view, &ui_view, (w, h), self.ctx.pixels_per_point(), output, &self.ctx);
        let row = (w * 4).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("agent apps readback"),
            size: u64::from(row * h),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo { texture: &texture, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(row), rows_per_image: None },
            },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
        queue.submit([encoder.finish()]);
        buffer.map_async(wgpu::MapMode::Read, .., |r| r.expect("mapping the snapshot buffer"));
        device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
        let data = buffer.slice(..).get_mapped_range().unwrap();
        let mut pixels = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            let start = (y * row) as usize;
            pixels.extend_from_slice(&data[start..start + (w * 4) as usize]);
        }
        drop(data);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        image::RgbaImage::from_raw(w, h, pixels).unwrap().save(&path).unwrap();
        eprintln!("wrote {}", path.display());
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        // Chrome, the shells and the desktop's apps end first, so nothing
        // writes into this test's data directory while it goes.
        let idle = crate::ui::browser::Browsers::new(self.data.clone(), Arc::new(|| {}));
        drop(std::mem::replace(&mut self.core.browsers, idle));
        self.core.close_terminals(AGENT);
        self.core.desktops.remove(AGENT);
        let _ = std::fs::remove_dir_all(&self.data);
    }
}

fn log(result: &AppResult) -> (String, String) {
    let log = result.log.as_ref().unwrap_or_else(|| panic!("a log line: {result:?}"));
    (log.tool.clone(), log.summary.clone())
}

#[test]
fn the_terminal_types_runs_and_reads() {
    let Some(mut h) = Harness::new("terminal", r#"["chat", "terminal"]"#) else { return };
    let list = h.call(AppCall::List);
    assert!(list.ok, "{}", list.text);
    let list: Value = serde_json::from_str(&list.text).unwrap();
    let handles: Vec<&str> = list["apps"].as_array().unwrap().iter().map(|a| a["handle"].as_str().unwrap()).collect();
    assert_eq!(handles, ["chat", "terminal:1"]);
    assert_eq!(list["apps"][1]["control"], "nobody");
    assert!(h.until(Duration::from_secs(10), |h| h.core.terminal(AGENT, 1).unwrap().term.text(0).contains('$')), "the prompt");

    let ran = h.act("terminal:1", "run", json!({ "command": "echo hi-$((40 + 2))" }));
    assert!(ran.ok, "{}", ran.text);
    assert!(ran.text.contains("hi-42"), "run answers with the new end of the screen: {}", ran.text);
    assert_eq!(log(&ran), ("terminal.run".into(), "echo hi-$((40 + 2))".into()));
    assert_eq!(doing(&h.core, AGENT, AppTab::Terminal(1)), Some("typing"), "the tile head says Claude is typing");
    h.snapshot("terminal-claude-is-typing");

    let typed = h.act("terminal:1", "type", json!({ "text": "echo typed-$((1 + 2))" }));
    assert!(typed.ok && log(&typed).0 == "terminal.type", "{typed:?}");
    let pressed = h.act("terminal:1", "key", json!({ "keys": "enter" }));
    assert_eq!(log(&pressed), ("terminal.key".into(), "enter".into()));
    assert!(h.until(Duration::from_secs(10), |h| h.core.terminal(AGENT, 1).unwrap().term.text(0).contains("typed-3")));

    let text = h.read("terminal:1", None);
    assert_eq!((text["handle"].as_str(), text["kind"].as_str()), (Some("terminal:1"), Some("terminal")));
    let content = text["content"].as_str().unwrap();
    assert!(content.contains("hi-42") && content.contains("typed-3"), "{content}");
    assert_eq!(text["extra"]["started_in"], json!(h.dir().display().to_string()));
    let found = h.read("terminal:1", Some("typed"));
    assert!(found["content"].as_str().unwrap().lines().all(|l| l.contains("typed")), "{found}");

    let bad = h.act("terminal:1", "key", json!({ "keys": "ctrl+nothing" }));
    assert!(!bad.ok && bad.text.contains("is not a key"), "{}", bad.text);
    let wrong = h.act("terminal:1", "click", json!({ "ref": "e1" }));
    assert!(!wrong.ok && wrong.text.contains("takes: type, key, run, focus, close"), "{}", wrong.text);
    let missing = h.act("terminal:3", "run", json!({ "command": "ls" }));
    assert!(!missing.ok && missing.text.contains("There is no terminal:3 open"), "{}", missing.text);
    let list: Value = serde_json::from_str(&h.call(AppCall::List).text).unwrap();
    assert_eq!(list["apps"][1]["control"], "you", "the agent acted on it just now");

    // Take over stops every act on the instance, reading still works (D12).
    h.core.terminal(AGENT, 1).unwrap().taken_over = true;
    let refused = h.act("terminal:1", "run", json!({ "command": "echo no" }));
    assert!(!refused.ok && refused.text.contains("the user took over this app"), "{}", refused.text);
    assert!(h.call(AppCall::Read { app: "terminal:1".into(), query: None }).ok);
    let list: Value = serde_json::from_str(&h.call(AppCall::List).text).unwrap();
    assert_eq!(list["apps"][1]["control"], "user");
    h.core.terminal(AGENT, 1).unwrap().taken_over = false;

    // app_open, focus and close.
    let selected = h.core.ui.tab;
    let opened = h.call(AppCall::Open { app: "terminal".into(), args: Value::Null });
    assert!(opened.ok && opened.text.starts_with("Opened terminal:2."), "{}", opened.text);
    assert_eq!(log(&opened), ("terminal.open".into(), "terminal:2".into()));
    assert_eq!(h.core.ui.tab, selected, "an agent's open does not take your selected tab");
    let focused = h.act("terminal:2", "focus", Value::Null);
    assert!(focused.ok);
    assert_eq!(h.core.ui.tab, AppTab::Terminal(2), "focus selects it in the view you look at");
    let closed = h.act("terminal:2", "close", Value::Null);
    assert!(closed.ok, "{}", closed.text);
    assert_eq!(crate::ui::agent::open_tabs(&h.core, AGENT), [AppTab::Chat, AppTab::Terminal(1)]);
    let chat = h.act("chat", "close", Value::Null);
    assert!(!chat.ok && chat.text.contains("cannot be closed"));
    let again = h.call(AppCall::Open { app: "chat".into(), args: Value::Null });
    assert!(again.ok && again.text.contains("open already"), "{}", again.text);
    let shot = h.call(AppCall::Screenshot { app: "terminal:1".into() });
    assert!(!shot.ok && shot.text.contains("Read it with app_read"), "{}", shot.text);
    let chat = h.read("chat", None);
    assert_eq!(chat["kind"], "chat");
}

#[test]
fn the_editor_files_and_viewer_open_paths_inside_the_directory() {
    let Some(mut h) = Harness::new("files", r#"["chat", "editor", "files", "viewer"]"#) else { return };
    let dir = h.dir();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    let source: String = (1..=10).map(|i| format!("line {i}\n")).collect();
    std::fs::write(dir.join("src/main.rs"), &source).unwrap();
    std::fs::write(dir.join("notes.md"), "# Notes\n\nThe plan.\n").unwrap();
    let outside = h.data.join("secret.txt");
    std::fs::write(&outside, "secret").unwrap();
    std::os::unix::fs::symlink(&outside, dir.join("link.txt")).unwrap();

    let opened = h.act("editor:1", "open", json!({ "path": "src/main.rs" }));
    assert!(opened.ok, "{}", opened.text);
    assert_eq!(log(&opened), ("editor.open".into(), "src/main.rs".into()));
    let text = h.read("editor:1", None);
    assert_eq!(text["title"], "src/main.rs");
    assert!(text["content"].as_str().unwrap().contains("    3  line 3"), "{text}");
    assert_eq!(text["extra"]["unsaved"], json!([]));
    let went = h.act("editor:1", "goto", json!({ "line": 7 }));
    assert!(went.ok, "{}", went.text);
    assert_eq!(h.read("editor:1", None)["extra"]["cursor"]["line"], 7);
    assert!(!h.act("editor:1", "goto", json!({ "line": 99 })).ok);

    for bad in ["../secret.txt", "link.txt", outside.to_str().unwrap(), "missing.rs"] {
        let refused = h.act("editor:1", "open", json!({ "path": bad }));
        assert!(!refused.ok, "{bad} stays outside");
        assert!(refused.text.contains("outside your directory") || refused.text.contains("does not exist"), "{}", refused.text);
    }

    let folder = h.act("files:1", "open", json!({ "path": "src" }));
    assert!(folder.ok, "{}", folder.text);
    let listing = h.read("files:1", None);
    assert_eq!(listing["extra"]["folder"], "src");
    assert!(listing["content"].as_str().unwrap().contains("main.rs"), "{listing}");
    assert_eq!(listing["elements"][0]["name"], "src/main.rs");

    let shown = h.act("viewer:1", "open", json!({ "path": "notes.md" }));
    assert!(shown.ok, "{}", shown.text);
    let notes = h.read("viewer:1", None);
    assert!(notes["content"].as_str().unwrap().contains("The plan."), "{notes}");

    // A file opened from Files goes to the editor or the viewer, like a double click.
    let from_files = h.act("files:1", "open", json!({ "path": "notes.md" }));
    assert!(from_files.ok && from_files.text.contains("viewer:1"), "{}", from_files.text);
    let editor2 = h.call(AppCall::Open { app: "editor".into(), args: json!({ "path": "src/main.rs" }) });
    assert!(editor2.ok && editor2.text.contains("editor:2"), "{}", editor2.text);

    // Take over of the editor refuses acts (D12).
    crate::ui::editor::tests_take_over(&mut h.core, AGENT, 1);
    let refused = h.act("editor:1", "goto", json!({ "line": 2 }));
    assert!(!refused.ok && refused.text.contains("took over"), "{}", refused.text);
}

#[test]
fn the_browser_opens_reads_clicks_types_and_takes_screenshots() {
    if meerora_browser::find_chrome(None).is_none() {
        eprintln!("skipped: Chrome is not installed");
        return;
    }
    let Some(mut h) = Harness::new("browser", r#"["chat", "browser"]"#) else { return };
    let html = "<title>Shop</title><h1>Welcome to the shop</h1>\
                <button onclick=\"document.title='Signed in';document.querySelector('h1').textContent='Hello again'\">Sign in</button>\
                <label>Search <input id=q></label>";
    // An agent's URL has no spaces (url::for_agent).
    let page = format!("data:text/html,{}", html.replace('%', "%25").replace(' ', "%20").replace('#', "%23").replace('"', "%22"));
    let opened = h.act("browser:1", "open", json!({ "url": page }));
    assert!(opened.ok, "{}", opened.text);
    assert_eq!(log(&opened).0, "browser.open");
    assert!(opened.text.contains("Shop"), "{}", opened.text);
    assert!(
        h.core.agent_apps.echoes.is_empty(),
        "the daemon's line for the open is known as the act's own, so browser 1 does not load it again"
    );

    let text = h.read("browser:1", None);
    assert_eq!(text["title"], "Shop");
    assert!(text["content"].as_str().unwrap().contains("Welcome to the shop"), "{text}");
    let elements = text["elements"].as_array().unwrap();
    let find = |role: &str| elements.iter().find(|e| e["role"] == role).unwrap_or_else(|| panic!("a {role} in {elements:?}")).clone();
    let button = find("button");
    assert_eq!(button["name"], "Sign in");
    let input = find("textbox");

    let clicked = h.act("browser:1", "click", json!({ "ref": button["ref"] }));
    assert!(clicked.ok, "{}", clicked.text);
    assert_eq!(log(&clicked), ("browser.click".into(), "Sign in".into()));
    assert!(clicked.text.contains("Signed in"), "the answer says where the page is now: {}", clicked.text);
    assert!(h.read("browser:1", None)["content"].as_str().unwrap().contains("Hello again"));
    h.snapshot("browser-after-agent-click");

    let typed = h.act("browser:1", "type", json!({ "ref": input["ref"], "text": "lamps", "submit": false }));
    assert!(typed.ok, "{}", typed.text);
    assert_eq!(log(&typed).1, input["name"].as_str().unwrap(), "the log names the field, not the text");
    let after = h.read("browser:1", Some("search"));
    assert!(after["elements"].as_array().unwrap().iter().any(|e| e["value"] == "lamps"), "{after}");

    let shot = h.call(AppCall::Screenshot { app: "browser:1".into() });
    assert!(shot.ok, "{}", shot.text);
    let png = base64::engine::general_purpose::STANDARD.decode(shot.image.unwrap()).unwrap();
    assert_eq!(&png[..4], b"\x89PNG");
    assert!(shot.text.starts_with("A picture of browser:1"));

    let refused = h.act("browser:1", "open", json!({ "url": "file:///etc/passwd" }));
    assert!(!refused.ok && refused.text.contains("does not open"), "{}", refused.text);
    let stale = h.act("browser:1", "click", json!({ "ref": "e999" }));
    assert!(!stale.ok, "{}", stale.text);

    // Take over stops the agent (D12).
    crate::ui::browser::tests_take_over(&mut h.core, AGENT, 1);
    let blocked = h.act("browser:1", "reload", Value::Null);
    assert!(!blocked.ok && blocked.text.contains("took over"), "{}", blocked.text);
}

#[test]
fn a_window_reads_types_and_shows_its_picture() {
    if !installed("weston-terminal") {
        eprintln!("skipped: weston-terminal is not installed");
        return;
    }
    let Some(mut h) = Harness::new("window", r#"["chat"]"#) else { return };
    let home = h.data.join("home").display().to_string();
    let command = ["env", &format!("HOME={home}"), "HISTFILE=/dev/null", "weston-terminal", "--shell=/bin/sh"].map(String::from);
    crate::desktop::snapshot_launch(&mut h.core, &h.ctx, &h.gpu.device, &h.gpu.queue, screen(), 1.0, &command).unwrap();
    let id = h.core.desktops.windows().iter().find(|w| w.desktop == AGENT).map(|w| w.id).expect("a window on Claude's desktop");
    let handle = format!("window:{id}");

    let text = h.read(&handle, None);
    assert_eq!(text["kind"], "window");
    assert_eq!(text["extra"]["sandboxed"], false, "you started it");
    assert!(text["summary"].as_str().unwrap().contains("app_screenshot"));
    let picture = |h: &mut Harness| {
        let shot = h.call(AppCall::Screenshot { app: handle.clone() });
        assert!(shot.ok, "{}", shot.text);
        let png = base64::engine::general_purpose::STANDARD.decode(shot.image.unwrap()).unwrap();
        image::load_from_memory(&png).unwrap().to_rgba8()
    };
    let before = picture(&mut h);
    assert!(before.width() > 100 && before.height() > 100);

    let typed = h.act(&handle, "type", json!({ "text": "echo AGENT-TYPED-THIS-LINE" }));
    assert!(typed.ok, "{}", typed.text);
    assert_eq!(log(&typed), ("window.type".into(), "echo AGENT-TYPED-THIS-LINE".into()));
    assert!(h.act(&handle, "key", json!({ "keys": "enter" })).ok);
    let changed = h.until(Duration::from_secs(10), |h| picture(h) != before);
    assert!(changed, "the typed line changed the window's picture");
    assert_eq!(h.core.desktops.focused(), None, "the agent's typing leaves the keyboard where it was");

    let clicked = h.act(&handle, "click", json!({ "x": 20, "y": 20 }));
    assert!(clicked.ok, "{}", clicked.text);
    assert_eq!(log(&clicked).0, "window.click");
    assert!(h.core.agent_apps.last_click(AGENT, id).is_some(), "the agent's cursor shows there");
    assert!(!h.act(&handle, "click", json!({ "x": 99999, "y": 1 })).ok, "outside the window");
    let umlaut = h.act(&handle, "type", json!({ "text": "grün" }));
    assert!(!umlaut.ok && umlaut.text.contains("cannot type"), "{}", umlaut.text);

    h.core.agent_apps.set_taken(AGENT, AppTab::Window(id), true);
    let refused = h.act(&handle, "type", json!({ "text": "no" }));
    assert!(!refused.ok && refused.text.contains("took over"), "{}", refused.text);
}

#[test]
fn an_agent_starts_a_real_app_in_its_sandbox() {
    if !installed("weston-terminal") || !installed("bwrap") {
        eprintln!("skipped: weston-terminal or bwrap is not installed");
        return;
    }
    let Some(mut h) = Harness::new("launch", r#"["chat"]"#) else { return };
    let opened = h.call(AppCall::Open { app: "weston-terminal".into(), args: json!({ "args": ["--shell=/bin/sh"] }) });
    if !opened.ok && (opened.text.contains("cannot start") || opened.text.contains("no window")) {
        eprintln!("skipped: the sandbox does not run here ({})", opened.text);
        return;
    }
    assert!(opened.ok, "{}", opened.text);
    assert!(opened.text.contains("in your sandbox, without network"), "{}", opened.text);
    let (tool, _) = log(&opened);
    assert_eq!(tool, "window.open");
    let id: u32 = opened.text.rsplit("window:").next().unwrap().trim_end_matches('.').parse().unwrap();
    assert!(h.core.desktops.window(id).is_some_and(|w| w.sandboxed && w.desktop == AGENT));
    let unknown = h.call(AppCall::Open { app: "no-such-program-for-meerora".into(), args: Value::Null });
    assert!(!unknown.ok && unknown.text.contains("neither a built-in app"), "{}", unknown.text);
}

#[test]
fn read_answers_are_filtered_and_cut() {
    let long = (0..2000).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
    let text = AppText { kind: "terminal".into(), content: long.clone(), keep_end: true, ..Default::default() };
    let out: Value = serde_json::from_str(&render(text, "terminal:1", None)).unwrap();
    let content = out["content"].as_str().unwrap();
    assert!(content.starts_with('[') && content.contains("more characters cut"), "{}", &content[..80]);
    assert!(content.ends_with("line 1999"), "the newest lines stay");
    assert_eq!(out["handle"], "terminal:1");

    let page = AppText {
        kind: "browser".into(),
        content: long,
        elements: (0..200)
            .map(|i| Element { r#ref: format!("e{i}"), role: "link".into(), name: format!("Item {i}"), value: None })
            .collect(),
        ..Default::default()
    };
    let out: Value = serde_json::from_str(&render(page.clone(), "browser:1", None)).unwrap();
    assert!(out["content"].as_str().unwrap().starts_with("line 0"), "a page keeps its start");
    assert_eq!(out["elements"].as_array().unwrap().len(), ELEMENTS_MAX);
    assert!(out["extra"]["elements_cut"].as_str().unwrap().contains("50 more"));
    let out: Value = serde_json::from_str(&render(page, "browser:1", Some("Item 19"))).unwrap();
    let names: Vec<&str> = out["elements"].as_array().unwrap().iter().map(|e| e["name"].as_str().unwrap()).collect();
    assert_eq!(names.len(), 12, "Item 19, 119 and 190 to 199: {names:?}");
    assert_eq!(out["content"], "[No line contains \"Item 19\".]");
}

#[test]
fn paths_stay_inside_the_directory() {
    let base = std::env::temp_dir().join(format!("meerora-inside-{}", std::process::id()));
    let dir = base.join("agent");
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    std::fs::write(dir.join("sub/a.txt"), "").unwrap();
    std::fs::write(base.join("other.txt"), "").unwrap();
    std::os::unix::fs::symlink(base.join("other.txt"), dir.join("out")).unwrap();
    std::os::unix::fs::symlink(dir.join("sub/a.txt"), dir.join("in")).unwrap();
    assert_eq!(inside(&dir, "sub/a.txt").unwrap(), PathBuf::from("sub/a.txt"));
    assert_eq!(inside(&dir, dir.join("sub").to_str().unwrap()).unwrap(), PathBuf::from("sub"));
    assert_eq!(inside(&dir, "in").unwrap(), PathBuf::from("sub/a.txt"), "a link inside resolves");
    assert!(inside(&dir, "out").unwrap_err().contains("outside"));
    assert!(inside(&dir, "../other.txt").unwrap_err().contains("outside"));
    assert!(inside(&dir, "/etc/hostname").is_err());
    assert!(inside(&dir, "").is_err());
    std::fs::remove_dir_all(&base).unwrap();
}

#[test]
fn the_git_app_reads_stages_and_unstages() {
    if !installed("git") {
        eprintln!("skipped: git is not installed");
        return;
    }
    let git = |dir: &Path, args: &[&str]| {
        let home = dir.parent().unwrap().parent().unwrap().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let ok = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("HOME", &home)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.org")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.org")
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?}");
    };
    let setup = |dir: &Path| {
        git(dir, &["init", "-q"]);
        std::fs::write(dir.join("notes.txt"), "one\n").unwrap();
        git(dir, &["add", "-A"]);
        git(dir, &["commit", "-q", "-m", "start"]);
        std::fs::write(dir.join("notes.txt"), "one\ntwo\n").unwrap();
    };
    let Some(mut h) = Harness::new_with("git", r#"["chat", "git"]"#, setup) else { return };
    let loaded = h.until(Duration::from_secs(20), |h| {
        crate::ui::apps::git::read(&h.core, AGENT, 1).is_some_and(|v| v["content"].as_str().is_some_and(|c| c.contains("notes.txt")))
    });
    assert!(loaded, "the Git app shows the agent's directory: {:?}", crate::ui::apps::git::read(&h.core, AGENT, 1));
    let text = h.read("git:1", None);
    assert_eq!(text["kind"], "git");
    assert!(text["elements"].as_array().unwrap().iter().any(|e| e["name"] == "notes.txt" && e["value"] == "changed"), "{text}");

    let staged = h.act("git:1", "stage", json!({ "path": "notes.txt" }));
    assert!(staged.ok, "{}", staged.text);
    assert_eq!(log(&staged), ("git.stage".into(), "notes.txt".into()));
    let has = |h: &mut Harness, value: &str| {
        h.until(Duration::from_secs(10), |h| {
            crate::ui::apps::git::read(&h.core, AGENT, 1).is_some_and(|v| {
                v["elements"].as_array().is_some_and(|l| l.iter().any(|e| e["name"] == "notes.txt" && e["value"] == value))
            })
        })
    };
    assert!(has(&mut h, "staged"), "the Git app shows it staged");
    let unstaged = h.act("git:1", "unstage", json!({ "path": "notes.txt" }));
    assert!(unstaged.ok, "{}", unstaged.text);
    assert!(has(&mut h, "changed"));
    let push = h.act("git:1", "push", Value::Null);
    assert!(!push.ok && push.text.contains("stage, unstage, commit"), "{}", push.text);
}
