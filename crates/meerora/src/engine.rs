//! Everything the window and the snapshot mode share: the world, the UI
//! state, assets, the daemon link, terminals and both renderers.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use meerora_geom::{Vec2, Vec3};
use meerora_protocol::{AgentDir, Event, RingState};
use meerora_scene::{Camera, FrameStats, Renderer, RendererConfig};
use meerora_term::{Spawn, TermSize, Terminal};

use crate::assets::{Assets, Change};
use crate::daemon::{DaemonLink, LinkMsg};
use crate::portraits::Portraits;
use crate::preview::{Preview, PreviewInput};
use crate::ui::{self, UiState, View};
use crate::world::{Agent, Figure, Space, World};

pub type Wake = Arc<dyn Fn() + Send + Sync>;

pub struct TerminalTile {
    pub term: Terminal,
    /// D12: the user took over, agent input is ignored until handed back.
    pub taken_over: bool,
}

pub struct Core {
    pub world: World,
    pub ui: UiState,
    pub assets: Assets,
    pub link: Option<DaemonLink>,
    pub scene: Renderer,
    pub egui_renderer: egui_wgpu::Renderer,
    pub portraits: Portraits,
    pub preview: Preview,
    pub last_stats: FrameStats,
    /// Agent directory created by the builder, loaded on the next step.
    pub pending_agent: Option<String>,
    /// Headless Chrome per agent for the browser tile (D13).
    pub browsers: ui::browser::Browsers,
    /// Devices of the space and of agents (D7, D8).
    pub devices: crate::devices::Devices,
    /// Retired agents and their tombstones in the garden.
    pub graveyard: crate::graveyard::Graveyard,
    /// The agents' desktops of the nested compositor, for real apps as tiles (D15 phase 2).
    pub desktops: crate::desktop::Desktops,
    /// App requests of agents and what the tile heads show about them (D30, agent_apps.rs).
    pub agent_apps: crate::agent_apps::AgentApps,
    /// Terminal instances by agent and instance number (`AppTab::Terminal`).
    terminals: HashMap<(String, u32), TerminalTile>,
    failed_terminals: HashSet<(String, u32)>,
    /// What a new terminal runs, before its directory is set. The default is
    /// the user's login shell. Tests put in a program and their own HOME.
    pub terminal_spawn: Spawn,
    agent_dirs: HashMap<String, AgentDir>,
    wake: Wake,
}

impl Core {
    pub fn new(device: &wgpu::Device, format: wgpu::TextureFormat, assets: Assets, link: Option<DaemonLink>, wake: Wake) -> Self {
        let scene = Renderer::new(device, RendererConfig { color_format: format, sample_count: 4 });
        // egui blends in gamma space, so it draws through a non-sRGB view of the same texture.
        let egui_renderer = egui_wgpu::Renderer::new(device, format.remove_srgb_suffix(), egui_wgpu::RendererOptions::default());
        let browsers = ui::browser::Browsers::new(assets.data_dir.clone(), wake.clone());
        let desktops = crate::desktop::Desktops::new(&assets.data_dir);
        Self {
            world: World::new(),
            ui: UiState::new(),
            assets,
            link,
            scene,
            egui_renderer,
            portraits: Portraits::new(device),
            preview: Preview::new(device),
            last_stats: FrameStats::default(),
            pending_agent: None,
            browsers,
            devices: crate::devices::Devices::default(),
            graveyard: crate::graveyard::Graveyard::default(),
            desktops,
            agent_apps: Default::default(),
            terminals: HashMap::new(),
            failed_terminals: HashSet::new(),
            terminal_spawn: Spawn::default(),
            agent_dirs: HashMap::new(),
            wake,
        }
    }

    /// Loads all agents, their figures and the space.
    pub fn load(&mut self, device: &wgpu::Device, queue: &wgpu::Queue) {
        for entry in meerora_protocol::load_agents(&self.assets.data_dir) {
            match entry {
                Ok(dir) => {
                    self.world.add_agent(Agent::new(&dir));
                    let id = dir.id.clone();
                    self.agent_dirs.insert(id.clone(), dir);
                    self.rebuild_figure(device, queue, &id);
                }
                Err(e) => self.ui.toast(0.0, e, true),
            }
        }
        self.rebuild_space(device);
        self.rebuild_devices(device);
    }

    /// Builds the device scripts again (D7).
    fn rebuild_devices(&mut self, device: &wgpu::Device) {
        for error in self.devices.rebuild(&mut self.scene, device, &self.assets.data_dir) {
            self.ui.toast(self.world.time, error, true);
        }
    }

    pub fn agent_dir(&self, id: &str) -> Option<&AgentDir> {
        self.agent_dirs.get(id)
    }

    /// Adds an agent directory another process wrote, such as an import from
    /// the index (D25), so the daemon's events for it apply at once. The
    /// figure is built on the next step.
    pub fn adopt_agent(&mut self, dir: AgentDir) {
        if self.world.agent_index(&dir.id).is_none() {
            self.world.add_agent(Agent::new(&dir));
        }
        self.pending_agent = Some(dir.id.clone());
        self.agent_dirs.insert(dir.id.clone(), dir);
    }

    fn rebuild_space(&mut self, device: &wgpu::Device) {
        match self.assets.build_space() {
            Ok(desc) => {
                let mesh = self.scene.upload_mesh(device, &desc.mesh);
                let walls = desc.walls.iter().map(|w| self.scene.upload_mesh(device, &w.mesh)).collect();
                if let Some(old) = self.world.space.take() {
                    self.scene.remove_mesh(old.mesh);
                    for wall in old.walls {
                        self.scene.remove_mesh(wall);
                    }
                }
                tracing::info!("space built: {} triangles, {} stations, {} walls", desc.triangles, desc.stations.len(), desc.walls.len());
                self.world.set_space(Space::new(desc, mesh, walls));
            }
            Err(e) => {
                tracing::warn!("space script failed: {e}");
                self.ui.toast(self.world.time, format!("space.js: {e}"), true);
            }
        }
    }

    fn rebuild_figure(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, id: &str) {
        let Some(dir) = self.agent_dirs.get(id) else { return };
        let result = self.assets.build_figure(dir);
        let Some(i) = self.world.agent_index(id) else { return };
        match result {
            Ok(desc) => {
                let base = self.scene.upload_mesh(device, &desc.base);
                let parts = desc.param_parts.iter().map(|p| self.scene.upload_mesh(device, &p.mesh)).collect();
                self.portraits.update(device, queue, &mut self.egui_renderer, id, &desc);
                let agent = &mut self.world.agents[i];
                if let Some(old) = agent.figure.take() {
                    self.scene.remove_mesh(old.base);
                    for m in old.parts {
                        self.scene.remove_mesh(m);
                    }
                }
                tracing::info!("figure {id} built: {} of {} triangles", desc.triangles, desc.budget);
                agent.figure = Some(Figure { desc, base, parts });
                agent.figure_error = None;
            }
            Err(e) => {
                tracing::warn!("figure {id} failed: {e}");
                // The previous figure stays, so a typo while editing does not make the agent vanish.
                self.world.agents[i].figure_error = Some(e.clone());
                self.ui.toast(self.world.time, format!("{id}/figure.js: {e}"), true);
            }
        }
    }

    fn reload_agent(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, id: &str) {
        let path = self.assets.data_dir.join("agents").join(id);
        match meerora_protocol::load_agent(&path) {
            Ok(dir) => {
                if self.world.agent_index(id).is_none() {
                    self.world.add_agent(Agent::new(&dir));
                }
                self.agent_dirs.insert(id.to_string(), dir);
                self.rebuild_figure(device, queue, id);
            }
            Err(e) => self.ui.toast(self.world.time, e, true),
        }
    }

    /// Advances one frame: daemon events, file changes, motion.
    pub fn step(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, dt: f32) {
        self.pump_link();
        if let Some(id) = self.pending_agent.take() {
            self.reload_agent(device, queue, &id);
        }
        for change in self.assets.poll() {
            match change {
                Change::Space => {
                    self.rebuild_space(device);
                    self.devices.mark_dirty();
                    self.graveyard.mark_dirty();
                }
                Change::Agent(id) => self.reload_agent(device, queue, &id),
            }
        }
        if self.devices.is_dirty() {
            self.rebuild_devices(device);
        }
        if self.graveyard.is_dirty() {
            self.rebuild_graveyard(device, queue);
        }
        self.desktops.dispatch();
        // Agents that face the viewer turn with the view.
        self.world.viewer = (self.ui.view == View::Space).then(|| self.scene_camera(Vec2::ONE).toward_viewer());
        self.world.update(dt);
        // The daemon waits for a retired agent to reach the graveyard (graveyard.rs of the daemon).
        for (agent, station) in std::mem::take(&mut self.world.arrivals) {
            if let Some(link) = &self.link {
                link.send(&meerora_protocol::Command::Arrived { agent, station });
            }
        }
        crate::agent_apps::step(self);
    }

    /// Applies what the daemon sent since the last call. It needs no GPU.
    fn pump_link(&mut self) {
        let mut events = Vec::new();
        if let Some(link) = &self.link {
            while let Ok(msg) = link.rx.try_recv() {
                match msg {
                    LinkMsg::Connected => {
                        self.ui.daemon_connected = true;
                        tracing::info!("connected to the agent daemon");
                    }
                    LinkMsg::Disconnected => {
                        self.ui.daemon_connected = false;
                        self.world.daemon_lost();
                        tracing::info!("agent daemon disconnected");
                    }
                    LinkMsg::Event(e) => events.push(e),
                }
            }
        }
        for e in events {
            self.apply(&e);
        }
    }

    /// Answers the app requests of agents between frames, for example while
    /// the meerora window is hidden and draws none (D30).
    pub fn serve_apps(&mut self) {
        self.pump_link();
        self.desktops.tick();
        crate::agent_apps::step(self);
    }

    /// Wakes the event loop, from any thread.
    pub fn waker(&self) -> Wake {
        self.wake.clone()
    }

    /// Applies a daemon event to the world and the terminals.
    pub fn apply(&mut self, e: &Event) {
        // App tool calls of agents go to agent_apps.rs, which answers them in step (D30).
        if crate::agent_apps::on_request(self, e) {
            return;
        }
        crate::ui::approvals::apply(&mut self.ui.approvals, e);
        crate::ui::config_card::on_event(self, e);
        if let Some((text, error)) = self.devices.apply(e) {
            self.ui.toast(self.world.time, text, error);
        }
        if crate::ui::index::on_event(self, e) {
            return;
        }
        if crate::ui::graveyard::on_event(self, e) {
            return;
        }
        if crate::ui::feed::on_event(self, e) {
            return;
        }
        if crate::ui::commands::on_event(self, e) {
            return;
        }
        if let Event::TerminalInput { agent, data } = e {
            // Milestone 1 has no approval flow for agent input yet (D8, D13), so
            // line breaks are dropped and nothing the mock types is executed.
            // The agent types into its first terminal.
            let text: String = data.chars().filter(|c| !c.is_control()).collect();
            if let Some(tile) = self.terminal(agent, 1)
                && !tile.taken_over
            {
                tile.term.write(text.as_bytes());
            } else {
                return;
            }
        }
        if let Event::FigureDraft { session, status, script, message, .. } = e {
            if let Some(b) = self.ui.builder.as_mut().filter(|b| b.session == *session) {
                crate::ui::builder::on_draft(b, *status, script.clone(), message.clone());
            }
            return;
        }
        if let Event::RunFinished { agent, outcome: meerora_protocol::Outcome::Failed, summary, .. } = e {
            let name = self.world.agent(agent).map_or(agent.clone(), |a| a.name.clone());
            self.ui.toast(self.world.time, format!("{name}: {}", summary.clone().unwrap_or_else(|| "the run failed".into())), true);
        }
        if let Event::Ring { agent, state, .. } = e
            && *state != RingState::NeedsYou
        {
            self.ui.dismissed.remove(agent);
        }
        if let Event::AgentRemoved { agent } = e {
            self.forget_agent(agent);
            return;
        }
        // An agent that appeared elsewhere, for example restored from the
        // graveyard by the command line, comes in from its directory.
        if let Event::AgentSpawned { agent, .. } = e
            && self.world.agent_index(agent).is_none()
            && let Ok(dir) = meerora_protocol::load_agent(&self.assets.data_dir.join("agents").join(agent))
        {
            self.adopt_agent(dir);
        }
        let echo = crate::agent_apps::take_echo(self, e);
        let _ = self.world.apply(e);
        // Browser 1 does not open again what an act of the agent opened.
        if echo && let Event::Action { agent, .. } = e {
            crate::ui::browser::mark_seen(self, agent, self.world.time);
        }
    }

    /// Frees what the app holds for an agent that left the space, for
    /// example into the graveyard: its terminals, its Chrome, its desktop
    /// with the apps on it, its editor state, its figure and its portrait.
    /// Its tile layout in `layouts.json` stays, so it gets it back when it
    /// returns.
    fn forget_agent(&mut self, id: &str) {
        self.browsers.close(id);
        self.desktops.remove(id);
        self.close_terminals(id);
        self.ui.editors.forget(id);
        self.ui.dismissed.remove(id);
        self.ui.approvals.requests.retain(|r| r.agent != id);
        if let Some(agent) = self.world.remove_agent(id)
            && let Some(figure) = agent.figure
        {
            self.scene.remove_mesh(figure.base);
            for part in figure.parts {
                self.scene.remove_mesh(part);
            }
        }
        self.portraits.remove(id, &mut self.egui_renderer);
        self.agent_dirs.remove(id);
        if self.pending_agent.as_deref() == Some(id) {
            self.pending_agent = None;
        }
        if self.ui.view == View::Agent(id.to_string()) {
            self.ui.view = View::Space;
        }
        if self.ui.builder.as_ref().is_some_and(|b| b.editing.as_deref() == Some(id)) {
            self.ui.builder = None;
            if self.ui.view == View::Builder {
                self.ui.view = View::Space;
            }
        }
        if self.ui.graveyard.confirm == Some(crate::ui::graveyard::Confirm::Retire { agent: id.to_string() }) {
            self.ui.graveyard.confirm = None;
        }
    }

    /// Builds the tombstones on the plot of the space and the portraits of
    /// the graveyard view again.
    fn rebuild_graveyard(&mut self, device: &wgpu::Device, queue: &wgpu::Queue) {
        let space = self.world.space.as_ref().map(|s| &s.desc);
        self.graveyard.rebuild(&mut self.scene, device, space);
        let keys: Vec<String> = self.graveyard.agents.iter().map(|a| crate::graveyard::portrait_key(&a.id)).collect();
        for old in std::mem::take(&mut self.graveyard.portraits) {
            if !keys.contains(&old) {
                self.portraits.remove(&old, &mut self.egui_renderer);
            }
        }
        let limits = meerora_script::Limits::default();
        for agent in self.graveyard.agents.clone() {
            let key = crate::graveyard::portrait_key(&agent.id);
            match meerora_script::build_figure(&agent.figure, "figure.js", &limits) {
                Ok(desc) => {
                    self.portraits.update(device, queue, &mut self.egui_renderer, &key, &desc);
                    self.graveyard.portraits.push(key);
                }
                Err(e) => tracing::info!("no portrait for {} in the graveyard ({e})", agent.id),
            }
        }
    }

    /// Terminal `n` of the agent, its own shell started in the agent's
    /// directory on first use (D21).
    pub fn terminal(&mut self, id: &str, n: u32) -> Option<&mut TerminalTile> {
        let key = (id.to_string(), n);
        if !self.terminals.contains_key(&key) && !self.failed_terminals.contains(&key) {
            let cwd = self.agent_dirs.get(id).map(|d| d.path.clone());
            match Terminal::spawn(Spawn { cwd, ..self.terminal_spawn.clone() }, TermSize { cols: 100, rows: 30 }) {
                Ok(term) => {
                    let wake = self.wake.clone();
                    term.set_wakeup(Box::new(move || wake()));
                    self.terminals.insert(key.clone(), TerminalTile { term, taken_over: false });
                }
                Err(e) => {
                    tracing::warn!("terminal {n} for {id} failed: {e}");
                    self.failed_terminals.insert(key.clone());
                }
            }
        }
        self.terminals.get_mut(&key)
    }

    /// Ends the shell of terminal `n` of an agent, when its app closes. The
    /// other terminals keep theirs. The next time terminal `n` opens it
    /// starts a new shell.
    pub fn close_terminal(&mut self, id: &str, n: u32) {
        let key = (id.to_string(), n);
        self.terminals.remove(&key);
        self.failed_terminals.remove(&key);
    }

    /// Ends the shells of all of an agent's terminals.
    pub fn close_terminals(&mut self, id: &str) {
        self.terminals.retain(|(agent, _), _| agent != id);
        self.failed_terminals.retain(|(agent, _)| agent != id);
    }

    /// The numbers of the agent's running terminals, lowest first.
    #[cfg(test)]
    pub fn terminal_instances(&self, id: &str) -> Vec<u32> {
        let mut out: Vec<u32> = self.terminals.keys().filter(|(agent, _)| agent == id).map(|(_, n)| *n).collect();
        out.sort_unstable();
        out
    }

    /// The scene camera for a viewport in points. In the space view the
    /// diorama is centred in the area right of the sidebar.
    pub fn scene_camera(&self, viewport: Vec2) -> Camera {
        let mut camera = self.world.camera;
        camera.aspect = viewport.x / viewport.y.max(1.0);
        if self.ui.view == View::Space {
            // The user may turn, move and zoom the view. Section 10 asked for
            // a fixed camera (see the deviation recorded there).
            camera.azimuth_deg = (camera.azimuth_deg + self.ui.view_yaw).rem_euclid(360.0);
            camera.elevation_deg = (camera.elevation_deg + self.ui.view_pitch).clamp(ui::space::MIN_ELEVATION, ui::space::MAX_ELEVATION);
            camera.view_height /= self.ui.view_zoom;
            let shift_points = ui::space::sidebar_right() / 2.0;
            let world_per_point = camera.view_height / viewport.y.max(1.0);
            let az = camera.azimuth_deg.to_radians();
            let right = Vec3::new(az.cos(), 0.0, -az.sin());
            camera.target -= right * shift_points * world_per_point;
            camera.target += self.ui.view_pan;
        }
        camera
    }

    /// Draws the builder preview into its texture (screen 3).
    pub fn render_preview(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, encoder: &mut wgpu::CommandEncoder) {
        if self.ui.view == View::Builder
            && let Some(b) = &self.ui.builder
            && let Some(figure) = &b.figure
        {
            self.preview.set_figure(device, b.figure_version, figure);
            let input = PreviewInput { figure, anim: b.anim, time: (self.world.time - b.anim_since) as f32, yaw: b.yaw, params: &b.params };
            self.preview.render(device, queue, encoder, &mut self.egui_renderer, b.preview_px, &input);
        }
        if self.ui.view == View::Index {
            crate::ui::index::render_preview(self, device, queue, encoder);
        }
    }

    pub fn preview_texture(&self) -> Option<egui::TextureId> {
        self.preview.texture()
    }

    pub fn build_ui(&mut self, ui: &mut egui::Ui) {
        ui::draw(ui, self);
    }

    /// Records the scene (space view only) and the egui output into `encoder`.
    /// `target` is the sRGB view of the frame, `ui_target` a non-sRGB view of
    /// the same texture for egui.
    #[allow(clippy::too_many_arguments)]
    pub fn paint(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        target: &wgpu::TextureView,
        ui_target: &wgpu::TextureView,
        size_px: (u32, u32),
        pixels_per_point: f32,
        mut output: egui::FullOutput,
        ctx: &egui::Context,
    ) {
        let viewport = Vec2::new(size_px.0 as f32 / pixels_per_point, size_px.1 as f32 / pixels_per_point);
        let mut frame = self.world.frame(self.scene_camera(viewport));
        self.devices.push_draws(&self.world, &mut frame);
        self.graveyard.push_draws(&mut frame);
        if self.ui.view != View::Space {
            frame.draws.clear();
            frame.rings.clear();
            frame.shadows.clear();
            frame.paths.clear();
        }
        self.last_stats = self.scene.render(device, queue, encoder, target, size_px, &frame);
        self.render_preview(device, queue, encoder);

        let screen = egui_wgpu::ScreenDescriptor { size_in_pixels: [size_px.0, size_px.1], pixels_per_point };
        for (id, deltas) in &output.textures_delta.set {
            for delta in deltas {
                self.egui_renderer.update_texture(device, queue, *id, delta);
            }
        }
        let primitives = ctx.tessellate(std::mem::take(&mut output.shapes), pixels_per_point);
        self.egui_renderer.update_buffers(device, queue, encoder, &primitives, &screen);
        {
            let pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("egui"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: ui_target,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            let mut pass = pass.forget_lifetime();
            self.egui_renderer.render(&mut pass, &primitives, &screen);
        }
        for id in &output.textures_delta.free {
            self.egui_renderer.free_texture(id);
        }
        output.textures_delta.clear();
    }

    /// Uploads texture changes of an egui pass that is not painted.
    pub fn apply_textures(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, mut delta: egui::TexturesDelta) {
        for (id, deltas) in &delta.set {
            for d in deltas {
                self.egui_renderer.update_texture(device, queue, *id, d);
            }
        }
        for id in &delta.free {
            self.egui_renderer.free_texture(id);
        }
        delta.clear();
    }
}
