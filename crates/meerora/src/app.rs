//! The window: winit event loop, wgpu surface and egui input (phase 1 of
//! D15, a fullscreen app inside the host session).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Context as _;
use winit::application::ApplicationHandler;
use winit::event::{ElementState, KeyEvent, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, KeyCode, ModifiersState, NamedKey, PhysicalKey};
use winit::window::{Fullscreen, Window, WindowId};

use crate::assets::Assets;
use crate::daemon::DaemonLink;
use crate::engine::{Core, Wake};
use crate::theme;

pub struct Options {
    pub data_dir: PathBuf,
    pub windowed: bool,
    pub daemon: bool,
    pub spawn_daemon: bool,
    /// `--session` makes meerora the login session (session.rs). It is always
    /// fullscreen, always asks for the desktop shortcuts and has a Log out button.
    pub session: bool,
}

struct Running {
    /// Desktop shortcuts for meerora (D15 phase 1). Before `window`, so it goes first.
    inhibitor: Option<crate::inhibit::Inhibitor>,
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    egui_ctx: egui::Context,
    egui_state: egui_winit::State,
    core: Core,
    last_frame: Instant,
    modifiers: ModifiersState,
    /// The log out button and question of `--session`, None in window mode.
    session: Option<crate::session::Session>,
}

struct App {
    options: Options,
    proxy: EventLoopProxy<()>,
    running: Option<Running>,
    error: Option<anyhow::Error>,
}

pub fn run(options: Options) -> anyhow::Result<()> {
    let event_loop = EventLoop::<()>::with_user_event().build().context("creating the event loop")?;
    event_loop.set_control_flow(ControlFlow::Poll);
    let proxy = event_loop.create_proxy();
    let mut app = App { options, proxy, running: None, error: None };
    event_loop.run_app(&mut app)?;
    app.error.map_or(Ok(()), Err)
}

/// The logo as the window icon on X11. Wayland compositors show the icon of
/// the desktop entry named by the app id instead, which the session installer adds.
fn window_icon() -> Option<winit::window::Icon> {
    let image = image::load_from_memory(include_bytes!("../../../assets/logo/meerora-logo-64.png")).ok()?.to_rgba8();
    let (width, height) = image.dimensions();
    winit::window::Icon::from_rgba(image.into_raw(), width, height).ok()
}

impl App {
    fn start(&mut self, event_loop: &ActiveEventLoop) -> anyhow::Result<Running> {
        let mut attrs = Window::default_attributes()
            .with_title("meerora")
            .with_inner_size(winit::dpi::LogicalSize::new(1440.0, 900.0))
            .with_window_icon(window_icon());
        {
            use winit::platform::wayland::WindowAttributesExtWayland;
            attrs = attrs.with_name("meerora", "meerora");
        }
        if !self.options.windowed {
            attrs = attrs.with_fullscreen(Some(Fullscreen::Borderless(None)));
        }
        let window = Arc::new(event_loop.create_window(attrs)?);

        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_with_display_handle(Box::new(event_loop.owned_display_handle())));
        let surface = instance.create_surface(window.clone())?;
        // D2: the integrated GPU is the target, so prefer it.
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::LowPower,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
            ..Default::default()
        }))
        .context("no GPU adapter for this window")?;
        tracing::info!("GPU: {} ({:?})", adapter.get_info().name, adapter.get_info().backend);
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("meerora"),
            required_limits: wgpu::Limits::downlevel_defaults().using_resolution(adapter.limits()),
            ..Default::default()
        }))?;

        let size = window.inner_size();
        let mut config =
            surface.get_default_config(&adapter, size.width.max(1), size.height.max(1)).context("surface not supported by the adapter")?;
        let caps = surface.get_capabilities(&adapter);
        if let Some(srgb) = caps.formats.iter().copied().find(|f| f.is_srgb()) {
            config.format = srgb;
        }
        config.present_mode = wgpu::PresentMode::AutoVsync;
        config.view_formats = vec![config.format.remove_srgb_suffix()];
        surface.configure(&device, &config);

        let egui_ctx = egui::Context::default();
        theme::install(&egui_ctx);
        let egui_state = egui_winit::State::new(
            egui_ctx.clone(),
            egui::ViewportId::ROOT,
            &window,
            Some(window.scale_factor() as f32),
            None,
            Some(device.limits().max_texture_dimension_2d as usize),
        );

        let proxy = self.proxy.clone();
        let wake: Wake = Arc::new(move || {
            let _ = proxy.send_event(());
        });
        let assets = {
            let wake = wake.clone();
            Assets::new(self.options.data_dir.clone(), move || wake())
        };
        let link = self.options.daemon.then(|| {
            let wake = wake.clone();
            DaemonLink::start(meerora_protocol::socket_path(), &self.options.data_dir, self.options.spawn_daemon, move || wake())
        });
        let desktop_wake = wake.clone();
        let mut core = Core::new(&device, config.format, assets, link, wake);
        // Nested apps keep running while the window is hidden and draws no frames (D15 phase 2).
        core.desktops.keep_alive(move || desktop_wake());
        core.load(&device, &queue);
        // Local voice runs only in the window, never in snapshots or tests (D16).
        crate::ui::voice::enable(&mut core);
        let inhibitor = if core.desktops.inhibit_shortcuts || self.options.session {
            crate::inhibit::Inhibitor::new(&window).map_err(|e| tracing::info!("desktop shortcuts stay with the desktop: {e}")).ok()
        } else {
            None
        };

        Ok(Running {
            inhibitor,
            window,
            surface,
            device,
            queue,
            config,
            egui_ctx,
            egui_state,
            core,
            last_frame: Instant::now(),
            modifiers: ModifiersState::empty(),
            session: self.options.session.then(crate::session::Session::default),
        })
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.running.is_some() {
            return;
        }
        match self.start(event_loop) {
            Ok(r) => self.running = Some(r),
            Err(e) => {
                self.error = Some(e);
                event_loop.exit();
            }
        }
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, _event: ()) {
        if let Some(r) = &mut self.running {
            // Daemon events, the nested apps and the app requests of agents (D30), also while hidden.
            r.core.serve_apps();
            r.window.request_redraw();
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let Some(r) = &mut self.running else { return };
        match &event {
            WindowEvent::ModifiersChanged(m) => r.modifiers = m.state(),
            WindowEvent::Focused(focused) => {
                r.core.desktops.host_focus(*focused);
                if *focused && let Some(i) = &mut r.inhibitor {
                    i.focused();
                }
            }
            _ => {}
        }
        // The command box (ui/commands): Alt+Space or `[commands] key` opens
        // and closes it over any view, also while an app window has the keyboard.
        if let WindowEvent::KeyboardInput { event: key, .. } = &event
            && let PhysicalKey::Code(code) = key.physical_key
            && crate::ui::commands::is_hotkey(&r.core, code, r.modifiers)
        {
            if key.state == ElementState::Pressed && !key.repeat {
                crate::ui::commands::toggle(&mut r.core);
                r.window.request_redraw();
            }
            return;
        }
        // While an app window has the keyboard, every key is its own, Esc
        // and F11 included, as physical keys before egui sees them (D15
        // phase 2). Alt+Tab stays meerora's, so leaving always works.
        if r.core.desktops.focused().is_some() {
            match &event {
                WindowEvent::KeyboardInput { event: key, .. } if !is_alt_tab(key, r.modifiers) => {
                    r.forward_key(key);
                    return;
                }
                WindowEvent::Ime(_) => return,
                _ => {}
            }
        }
        if let WindowEvent::KeyboardInput {
            event: KeyEvent { logical_key: Key::Named(NamedKey::F11), state: ElementState::Pressed, repeat: false, .. },
            ..
        } = &event
        {
            let full = r.window.fullscreen().is_some();
            r.window.set_fullscreen((!full).then_some(Fullscreen::Borderless(None)));
            return;
        }
        let response = r.egui_state.on_window_event(&r.window, &event);
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                r.config.width = size.width.max(1);
                r.config.height = size.height.max(1);
                r.surface.configure(&r.device, &r.config);
                r.window.request_redraw();
            }
            WindowEvent::RedrawRequested => {
                r.frame();
                // Log out in the session. Leaving the event loop ends cage and the session.
                if r.session.as_ref().is_some_and(|s| s.quit) {
                    event_loop.exit();
                }
            }
            _ => {
                if response.repaint {
                    r.window.request_redraw();
                }
            }
        }
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        // Rings turn and figures breathe all the time, so the space redraws every frame (vsync paced).
        if let Some(r) = &mut self.running {
            // A hidden window gets no redraws, so the nested apps are served here too.
            r.core.desktops.tick();
            r.window.request_redraw();
        }
    }
}

/// Alt+Tab (switch agents) and Alt+` (switch the agent's tabs), which reach
/// meerora even while an app window has the keyboard.
fn is_alt_tab(key: &KeyEvent, modifiers: ModifiersState) -> bool {
    matches!(key.physical_key, PhysicalKey::Code(KeyCode::Tab | KeyCode::Backquote)) && modifiers.alt_key()
}

impl Running {
    /// Sends a key to the app window with the keyboard. The app repeats
    /// held keys itself, so winit's repeats stay out. A paste first puts the
    /// host clipboard's text on the desktops' clipboard.
    fn forward_key(&mut self, key: &KeyEvent) {
        if key.repeat {
            return;
        }
        let Some(code) = crate::keymap::evdev(key.physical_key) else { return };
        let pressed = key.state == ElementState::Pressed;
        let paste = (key.physical_key == PhysicalKey::Code(KeyCode::KeyV) && self.modifiers.control_key())
            || (key.physical_key == PhysicalKey::Code(KeyCode::Insert) && self.modifiers.shift_key());
        if pressed
            && paste
            && let Some(text) = self.egui_state.clipboard_text()
        {
            self.core.desktops.set_selection(&text);
        }
        self.core.desktops.key(code, pressed);
    }

    fn frame(&mut self) {
        if let Some(i) = &mut self.inhibitor {
            i.dispatch();
        }
        let now = Instant::now();
        let dt = now.duration_since(self.last_frame).as_secs_f32().min(0.1);
        self.last_frame = now;
        if dt > 0.0 {
            self.core.ui.fps = self.core.ui.fps * 0.9 + (1.0 / dt) * 0.1;
        }
        self.core.step(&self.device, &self.queue, dt);

        let raw = self.egui_state.take_egui_input(&self.window);
        let core = &mut self.core;
        let session = &mut self.session;
        let mut output = self.egui_ctx.run_ui(raw, |ui| {
            if let Some(s) = session {
                s.draw(ui, core.ui.view == crate::ui::View::Space);
            }
            core.build_ui(ui);
        });
        let platform = std::mem::take(&mut output.platform_output);
        self.egui_state.handle_platform_output(&self.window, platform);

        let texture = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t) | wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.surface.configure(&self.device, &self.config);
                return;
            }
            other => {
                tracing::debug!("skipping frame: {other:?}");
                return;
            }
        };
        let view = texture.texture.create_view(&wgpu::TextureViewDescriptor::default());
        let ui_view = texture
            .texture
            .create_view(&wgpu::TextureViewDescriptor { format: Some(self.config.format.remove_srgb_suffix()), ..Default::default() });
        let mut encoder = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("frame") });
        let ppp = self.egui_ctx.pixels_per_point();
        self.core.paint(
            &self.device,
            &self.queue,
            &mut encoder,
            &view,
            &ui_view,
            (self.config.width, self.config.height),
            ppp,
            output,
            &self.egui_ctx,
        );
        self.queue.submit([encoder.finish()]);
        self.queue.present(texture);
    }
}
