//! `meerora snapshot out.png`: renders the full UI headless, for checking
//! changes without a window. Daemon events can be replayed from a JSON lines
//! file and time can be fast-forwarded so figures arrive at their stations.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context as _, bail};
use meerora_scene::{Gpu, IMAGE_FORMAT};

use crate::assets::Assets;
use crate::engine::Core;
use crate::theme;
use crate::ui::{AppTab, Layout, View};

pub struct Options {
    pub out: PathBuf,
    pub data_dir: PathBuf,
    pub events: Option<PathBuf>,
    /// Figure script loaded into the builder preview.
    pub builder_script: Option<PathBuf>,
    pub builder_step: u8,
    /// Result selected on the index screen, whose results come from `events`.
    pub index_select: usize,
    /// The interview so far for `--view profile` (turns, question, profile as JSON).
    pub interview: Option<PathBuf>,
    /// `--view profile:review`.
    pub review: bool,
    /// The device whose card is open in the space view (D7).
    pub device: Option<String>,
    /// Agent opened in the builder for editing.
    pub edit: Option<String>,
    /// Degrees the space view is turned and tilted, like a drag in the app.
    pub turn: (f32, f32),
    /// Opens the app launcher of the agent view.
    pub launcher: bool,
    /// Lists the retired agents of `data_dir/graveyard/`, as the daemon's greeting would.
    pub graveyard: bool,
    /// Asks whether this agent should move to the graveyard.
    pub retire: Option<String>,
    /// Asks whether this retired agent's folder should go to the trash.
    pub delete: Option<String>,
    /// Puts the pointer on the tombstone of this retired agent, for its label.
    pub hover_stone: Option<String>,
    /// Opens the room picker of the space card (ui/rooms.rs).
    pub rooms: bool,
    /// Opens "Ask an agent to build a device" with this text (ui/device_list.rs).
    pub ask_device: Option<String>,
    /// A program and its arguments to start on the shown agent's desktop, whose window shows as a tile.
    pub launch: Option<Vec<String>>,
    /// The Log out button of `--session`, or its question with `--logout`.
    pub session: Option<crate::session::Session>,
    /// A state of local voice to show (`--voice`, ui/voice.rs). Voice stays off without it.
    pub voice: Option<String>,
    /// Opens the command box with this query (`--commands`, ui/commands).
    pub commands: Option<String>,
    /// The finished quick answer of a `??` query in the box (`--answer`).
    pub answer: Option<String>,
    pub warp: f32,
    pub view: View,
    pub layout: Layout,
    pub tab: AppTab,
    /// App instances to open in the agent view, with a URL for browsers and a repository for Git.
    pub open: Vec<(AppTab, Option<String>)>,
    /// How Git instances show (`--git unified,staged`).
    pub git: Vec<String>,
    /// A commit message for the Git instances (`--git-message`).
    pub git_message: Option<String>,
    pub width: u32,
    pub height: u32,
    pub scale: f32,
    /// `--frames N`: N frames `OUT-0001.png`, `OUT-0002.png` and so on
    /// instead of one still, for videos (docs/video/). 0 renders a still.
    pub frames: u32,
    /// Frames per second, so each frame advances time by `1 / fps`.
    pub fps: f32,
    /// The turn of the last frame (`--turn-to`). The camera eases from `turn` to it.
    pub turn_to: Option<(f32, f32)>,
    /// The view zoom of the first frame or the still (`--zoom-from`).
    pub zoom_from: f32,
    /// The view zoom of the last frame (`--zoom-to`), `zoom_from` when unset.
    pub zoom_to: Option<f32>,
    /// The point of the screen, in points, that stays in place while the view zooms (`--zoom-at`).
    pub zoom_at: Option<(f32, f32)>,
    /// Events for given frames (`--inject`), one `FRAME EVENT` per line.
    pub inject: Option<PathBuf>,
}

pub fn run(o: Options) -> anyhow::Result<()> {
    let gpu = Gpu::headless()?;
    let (device, queue) = (&gpu.device, &gpu.queue);
    let mut core = Core::new(device, IMAGE_FORMAT, Assets::without_watcher(o.data_dir.clone()), None, Arc::new(|| {}));
    core.load(device, queue);

    if o.view == View::Index {
        crate::ui::index::open(&mut core);
    }
    // The view turns before the events, so agents that arrive at a shared
    // station take the slots the turned camera sees side by side (world.rs).
    core.ui.view_yaw = o.turn.0;
    core.ui.view_pitch = o.turn.1;
    place_camera(&mut core, &o, 0.0);
    core.world.viewer = Some(core.scene_camera(meerora_geom::Vec2::ONE).toward_viewer());
    if let Some(path) = &o.events {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        for (n, line) in text.lines().enumerate().filter(|(_, l)| !l.trim().is_empty() && !l.trim_start().starts_with("//")) {
            let event = meerora_protocol::decode_event(line).with_context(|| format!("{}:{}", path.display(), n + 1))?;
            core.apply(&event);
        }
        core.ui.daemon_connected = true;
    }
    if o.graveyard {
        core.apply(&meerora_protocol::Event::Graveyard { agents: meerora_protocol::load_graveyard(&o.data_dir) });
    }
    let steps = (o.warp * 30.0).round() as usize;
    for _ in 0..steps {
        core.step(device, queue, 1.0 / 30.0);
    }
    if let View::Agent(id) = &o.view
        && core.world.agent(id).is_none()
    {
        bail!("no agent {id:?} in {}", o.data_dir.display());
    }
    if let Some(id) = &o.edit {
        crate::ui::builder::open_edit(&mut core, id);
        if let Some(b) = core.ui.builder.as_mut() {
            b.step = o.builder_step;
        }
    } else if o.view == View::Builder {
        crate::ui::open_builder(&mut core);
        if let Some(b) = core.ui.builder.as_mut() {
            b.name = "Wren".into();
            if let Some(path) = &o.builder_script {
                let script = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
                b.messages
                    .push((meerora_protocol::Role::User, "A small mail agent, egg shaped, ochre felt, with a blue postal cap.".into()));
                b.messages.push((meerora_protocol::Role::Agent, format!("Here is a version from {}.", path.display())));
                b.accept(script);
                // Show every state switched on.
                let toggles: Vec<String> = b
                    .figure
                    .iter()
                    .flat_map(|f| f.params.iter())
                    .filter(|p| matches!(p.kind, meerora_script::ParamKind::Toggle { .. }))
                    .map(|p| p.name.clone())
                    .collect();
                for name in toggles {
                    b.params.insert(name, serde_json::Value::Bool(true));
                }
            }
        }
    } else {
        core.ui.view = o.view.clone();
    }
    if core.ui.view == View::Index {
        crate::ui::index::select(&mut core, o.index_select);
        crate::ui::index::wait_for_pictures(&mut core, std::time::Duration::from_secs(5));
    }
    if core.ui.view == View::Profile {
        match &o.interview {
            Some(path) => crate::ui::profile::snapshot_interview(&mut core, path).with_context(|| format!("reading {}", path.display()))?,
            None => {
                let agent = crate::ui::feed::personal_agent(&core).context("no personal agent in this data dir")?;
                crate::ui::profile::open_for(&mut core, &agent);
                crate::ui::profile::snapshot_review(&mut core);
            }
        }
        if o.review {
            crate::ui::profile::snapshot_review(&mut core);
        }
        crate::ui::profile::request_pictures(&mut core);
    }
    if matches!(core.ui.view, View::Feed | View::Profile | View::Index) {
        crate::ui::feed::wait_for_pictures(&mut core, std::time::Duration::from_secs(5));
    }
    core.ui.layout = o.layout;
    if let View::Agent(id) = core.ui.view.clone() {
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(o.width as f32 / o.scale, o.height as f32 / o.scale));
        let content = crate::ui::agent::content_rect(screen);
        for (tab, url) in &o.open {
            crate::ui::agent::open_app(&mut core, &id, *tab, content);
            if let (AppTab::Browser(n), Some(url)) = (tab, url) {
                crate::ui::browser::navigate(&mut core, &id, *n, url);
            }
            if let (AppTab::Git(n), Some(path)) = (tab, url) {
                let shown = crate::ui::apps::git::SnapshotGit {
                    repo: Some(PathBuf::from(path)),
                    unified: o.git.iter().any(|w| w == "unified"),
                    staged: o.git.iter().any(|w| w == "staged"),
                    message: o.git_message.clone(),
                };
                crate::ui::apps::git::snapshot(&mut core, &id, *n, &shown);
            }
        }
    }
    core.ui.tab = o.tab;
    core.ui.view_yaw = o.turn.0;
    core.ui.view_pitch = o.turn.1;
    place_camera(&mut core, &o, 0.0);
    core.ui.launcher = o.launcher;
    if let Some(id) = &o.device {
        if core.devices.device(id).is_none() {
            bail!("no device {id:?} in {}", o.data_dir.display());
        }
        core.devices.card = Some(id.clone());
    }
    if let Some(id) = &o.retire {
        if core.world.agent(id).is_none() {
            bail!("no agent {id:?} in {}", o.data_dir.display());
        }
        crate::ui::graveyard::ask_retire(&mut core, id);
    }
    if let Some(id) = &o.delete {
        if core.graveyard.agent(id).is_none() {
            bail!("no {id:?} in the graveyard of {}, pass --graveyard", o.data_dir.display());
        }
        core.ui.graveyard.confirm = Some(crate::ui::graveyard::Confirm::Delete { grave: id.clone() });
    }
    crate::ui::voice::snapshot(&mut core, o.voice.as_deref())?;
    if let Some(query) = &o.commands {
        crate::ui::commands::snapshot(&mut core, query, o.answer.as_deref())?;
    }

    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("snapshot"),
        size: wgpu::Extent3d { width: o.width, height: o.height, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: IMAGE_FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[IMAGE_FORMAT.remove_srgb_suffix()],
    });

    let ctx = egui::Context::default();
    theme::install(&ctx);
    if o.rooms {
        crate::ui::rooms::open(&ctx, &core);
    }
    if let Some(text) = &o.ask_device {
        crate::ui::device_list::open(&ctx, &core, text);
    }
    let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(o.width as f32 / o.scale, o.height as f32 / o.scale));
    let pointer = match &o.hover_stone {
        Some(id) => {
            let top = core.graveyard.top(id).with_context(|| format!("no tombstone for {id:?}, pass --graveyard"))?;
            let viewport = meerora_geom::Vec2::new(screen.width(), screen.height());
            let at =
                core.scene_camera(viewport).project(top - meerora_geom::Vec3::Y * 14.0, viewport).context("the tombstone is off screen")?;
            Some(egui::pos2(at.x, at.y))
        }
        None => None,
    };
    if let Some(command) = &o.launch {
        crate::desktop::snapshot_launch(&mut core, &ctx, device, queue, screen, o.scale, command)?;
    }
    // egui sizes some areas in the first pass, so the third pass is the one painted.
    let mut output = None;
    for _ in 0..3 {
        if matches!(core.ui.view, View::Agent(_)) {
            // Give the shell in the terminal tile time to print its prompt.
            std::thread::sleep(std::time::Duration::from_millis(400));
        }
        let out = ui_pass(&mut core, &ctx, &o, screen, pointer, None);
        // The builder preview needs the stage size from a UI pass before it can render.
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("preview") });
        core.render_preview(device, queue, &mut encoder);
        queue.submit([encoder.finish()]);
        if let Some(prev) = output.replace(out) {
            let prev: egui::FullOutput = prev;
            core.apply_textures(device, queue, prev.textures_delta);
        }
        // The first pass starts Chrome for a browser tile. Wait for its page.
        core.browsers.wait_settled(std::time::Duration::from_secs(20));
        // Git instances read their repository and diff on threads.
        crate::ui::apps::git::wait_settled(&mut core, std::time::Duration::from_secs(15));
    }
    let output = output.expect("three passes ran");
    if o.frames > 0 {
        return frames(&mut core, &ctx, device, queue, &o, screen, pointer, output, &texture);
    }
    let image = read_back(&mut core, &ctx, device, queue, &o, output, &texture)?;
    if let Some(dir) = o.out.parent() {
        std::fs::create_dir_all(dir)?;
    }
    image.save(&o.out).with_context(|| format!("writing {}", o.out.display()))?;
    println!("wrote {} ({} triangles, {} draw calls)", o.out.display(), core.last_stats.triangles, core.last_stats.draw_calls);
    Ok(())
}

/// One egui pass over the whole UI. `time` is the clock egui animates by,
/// with `dt` seconds per frame.
fn ui_pass(
    core: &mut Core,
    ctx: &egui::Context,
    o: &Options,
    screen: egui::Rect,
    pointer: Option<egui::Pos2>,
    time: Option<(f64, f32)>,
) -> egui::FullOutput {
    let mut raw = egui::RawInput { screen_rect: Some(screen), ..Default::default() };
    if let Some((now, dt)) = time {
        raw.time = Some(now);
        raw.predicted_dt = dt;
    }
    raw.events.extend(pointer.map(egui::Event::PointerMoved));
    raw.viewports.entry(egui::ViewportId::ROOT).or_default().native_pixels_per_point = Some(o.scale);
    let mut session = o.session.as_ref().map(|s| crate::session::Session { asking: s.asking, quit: false });
    ctx.run_ui(raw, |ui| {
        if let Some(s) = &mut session {
            s.draw(ui, core.ui.view == View::Space);
        }
        core.build_ui(ui);
    })
}

/// Eases the space view from the first frame (`t` = 0) to the last (`t` =
/// 1): turn, tilt and zoom, and the pan that keeps `zoom_at` in place, as
/// the scroll wheel does (ui/space.rs).
fn place_camera(core: &mut Core, o: &Options, t: f32) {
    let e = t * t * (3.0 - 2.0 * t);
    let lerp = |a: f32, b: f32| a + (b - a) * e;
    let (yaw, pitch) = o.turn_to.map_or(o.turn, |to| (lerp(o.turn.0, to.0), lerp(o.turn.1, to.1)));
    core.ui.view_yaw = yaw;
    core.ui.view_pitch = pitch;
    core.ui.view_zoom = lerp(o.zoom_from, o.zoom_to.unwrap_or(o.zoom_from)).clamp(0.5, 3.0);
    core.ui.view_pan = meerora_geom::Vec3::ZERO;
    if let Some((x, y)) = o.zoom_at {
        let viewport = meerora_geom::Vec2::new(o.width as f32 / o.scale, o.height as f32 / o.scale);
        let base = core.world.camera;
        let az = (base.azimuth_deg + yaw).to_radians();
        let el = (base.elevation_deg + pitch).clamp(crate::ui::space::MIN_ELEVATION, crate::ui::space::MAX_ELEVATION).to_radians();
        let right = meerora_geom::Vec3::new(az.cos(), 0.0, -az.sin());
        let up = meerora_geom::Vec3::new(-az.sin() * el.sin(), el.cos(), -az.cos() * el.sin());
        let (cx, cy) = (viewport.x / 2.0 + crate::ui::space::sidebar_right() / 2.0, viewport.y / 2.0);
        // The point sits this far from the centre at zoom 1. Zooming to z
        // keeps it in place when the view moves by (1 - 1/z) of that.
        let offset = (right * (x - cx) - up * (y - cy)) * (base.view_height / viewport.y.max(1.0));
        core.ui.view_pan = offset * (1.0 - 1.0 / core.ui.view_zoom);
    }
}

/// `--frames N`: advances time by 1/fps per frame, eases the camera and
/// applies the events of `--inject` at their frames. Writes `OUT-0001.png`
/// and on.
#[allow(clippy::too_many_arguments)]
fn frames(
    core: &mut Core,
    ctx: &egui::Context,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    o: &Options,
    screen: egui::Rect,
    pointer: Option<egui::Pos2>,
    warm: egui::FullOutput,
    texture: &wgpu::Texture,
) -> anyhow::Result<()> {
    core.apply_textures(device, queue, warm.textures_delta);
    let mut inject = match &o.inject {
        Some(path) => read_inject(path)?,
        None => Vec::new(),
    };
    inject.sort_by_key(|(frame, _)| *frame);
    let mut next = 0;
    let stem = o.out.file_stem().context("--frames needs an output name such as out.png")?.to_string_lossy().into_owned();
    let ext = o.out.extension().map_or("png".into(), |e| e.to_string_lossy().into_owned());
    if let Some(dir) = o.out.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let dt = 1.0 / o.fps;
    for f in 1..=o.frames {
        let t = if o.frames > 1 { (f - 1) as f32 / (o.frames - 1) as f32 } else { 0.0 };
        place_camera(core, o, t);
        while let Some((at, event)) = inject.get(next).filter(|(at, _)| *at <= f) {
            tracing::debug!("frame {at}: {event:?}");
            core.apply(event);
            next += 1;
        }
        if f > 1 {
            core.step(device, queue, dt);
        }
        let out = ui_pass(core, ctx, o, screen, pointer, Some((core.world.time, dt)));
        let image = read_back(core, ctx, device, queue, o, out, texture)?;
        let path = o.out.with_file_name(format!("{stem}-{f:04}.{ext}"));
        image.save(&path).with_context(|| format!("writing {}", path.display()))?;
    }
    println!("wrote {} frames {}-0001.{ext} to {stem}-{:04}.{ext}", o.frames, o.out.with_file_name(&stem).display(), o.frames);
    Ok(())
}

/// The lines of an `--inject` file: a frame number, a space and an event.
/// Empty lines and lines that start with `//` are skipped.
fn read_inject(path: &std::path::Path) -> anyhow::Result<Vec<(u32, meerora_protocol::Event)>> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut out = Vec::new();
    for (n, line) in text.lines().enumerate().filter(|(_, l)| !l.trim().is_empty() && !l.trim_start().starts_with("//")) {
        let at = || format!("{}:{}", path.display(), n + 1);
        let (frame, event) = line.trim().split_once(char::is_whitespace).with_context(|| format!("{}: FRAME EVENT", at()))?;
        let frame = frame.parse().with_context(|| format!("{}: the line starts with a frame number", at()))?;
        out.push((frame, meerora_protocol::decode_event(event.trim()).with_context(at)?));
    }
    Ok(out)
}

/// Paints the UI and the scene into `texture` and reads it back.
fn read_back(
    core: &mut Core,
    ctx: &egui::Context,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    o: &Options,
    output: egui::FullOutput,
    texture: &wgpu::Texture,
) -> anyhow::Result<image::RgbaImage> {
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let ui_view =
        texture.create_view(&wgpu::TextureViewDescriptor { format: Some(IMAGE_FORMAT.remove_srgb_suffix()), ..Default::default() });
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("snapshot") });
    core.paint(device, queue, &mut encoder, &view, &ui_view, (o.width, o.height), ctx.pixels_per_point(), output, ctx);

    let row = (o.width * 4).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("snapshot readback"),
        size: (row * o.height) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo { texture, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(row), rows_per_image: None },
        },
        wgpu::Extent3d { width: o.width, height: o.height, depth_or_array_layers: 1 },
    );
    queue.submit([encoder.finish()]);
    buffer.map_async(wgpu::MapMode::Read, .., |r| r.expect("mapping the snapshot buffer"));
    device.poll(wgpu::PollType::wait_indefinitely())?;
    let data = buffer.slice(..).get_mapped_range().map_err(|e| anyhow::anyhow!("mapping the snapshot buffer: {e:?}"))?;
    let mut pixels = Vec::with_capacity((o.width * o.height * 4) as usize);
    for y in 0..o.height {
        let start = (y * row) as usize;
        pixels.extend_from_slice(&data[start..start + (o.width * 4) as usize]);
    }
    drop(data);
    image::RgbaImage::from_raw(o.width, o.height, pixels).context("snapshot buffer size")
}
