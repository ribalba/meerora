//! The live figure preview of the builder (screen 3): one figure on a
//! platform, rendered every frame into a texture that egui shows.

use std::collections::HashMap;
use std::f32::consts::{PI, TAU};

use meerora_geom::{Mat4, Material, Quat, Vec3, prim};
use meerora_scene::{BlobShadow, Camera, Draw, Frame, MeshId, PointLight, Renderer, RendererConfig, Ring};
use meerora_script::{FigureDesc, Motion, ParamDef, ParamKind};

use crate::theme;
use crate::world::motion_matrix;

const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

/// Which animation the preview plays.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreviewAnim {
    Working,
    NeedsYou,
    Resting,
    Walk,
    Hop,
}

/// What to draw this frame.
pub struct PreviewInput<'a> {
    pub figure: &'a FigureDesc,
    pub anim: PreviewAnim,
    /// Seconds since the animation was picked.
    pub time: f32,
    /// Extra rotation from dragging, radians.
    pub yaw: f32,
    pub params: &'a HashMap<String, serde_json::Value>,
}

struct Target {
    _texture: wgpu::Texture,
    view: wgpu::TextureView,
    id: egui::TextureId,
    size: (u32, u32),
}

pub struct Preview {
    renderer: Renderer,
    platform: MeshId,
    figure: Option<(u64, MeshId, Vec<MeshId>)>,
    target: Option<Target>,
}

impl Preview {
    pub fn new(device: &wgpu::Device) -> Self {
        let mut renderer = Renderer::new(device, RendererConfig { color_format: FORMAT, sample_count: 4 });
        let mut platform = prim::cylinder(1.0, 1.0, 48).painted(&Material::from_hex("#1E222C").expect("valid colour"));
        let mut rim = prim::cylinder(1.04, 0.6, 48).painted(&Material::from_hex("#14171E").expect("valid colour"));
        rim.transform(Mat4::from_translation(Vec3::new(0.0, -0.6, 0.0)));
        platform.append(&rim);
        let platform = renderer.upload_mesh(device, &platform);
        Self { renderer, platform, figure: None, target: None }
    }

    /// The texture of the last frame, for egui.
    pub fn texture(&self) -> Option<egui::TextureId> {
        self.target.as_ref().map(|t| t.id)
    }

    /// Uploads the figure if `version` changed since the last call.
    pub fn set_figure(&mut self, device: &wgpu::Device, version: u64, figure: &FigureDesc) {
        if self.figure.as_ref().is_some_and(|(v, _, _)| *v == version) {
            return;
        }
        if let Some((_, base, parts)) = self.figure.take() {
            self.renderer.remove_mesh(base);
            for p in parts {
                self.renderer.remove_mesh(p);
            }
        }
        let base = self.renderer.upload_mesh(device, &figure.base);
        let parts = figure.param_parts.iter().map(|p| self.renderer.upload_mesh(device, &p.mesh)).collect();
        self.figure = Some((version, base, parts));
    }

    /// Renders into the preview texture, resizing it to `size` pixels first.
    #[allow(clippy::too_many_arguments)]
    pub fn render(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        egui_renderer: &mut egui_wgpu::Renderer,
        size: (u32, u32),
        input: &PreviewInput,
    ) {
        let Some((_, base, parts)) = &self.figure else { return };
        if size.0 < 8 || size.1 < 8 {
            return;
        }
        if self.target.as_ref().is_none_or(|t| t.size != size) {
            if let Some(old) = self.target.take() {
                egui_renderer.free_texture(&old.id);
            }
            let texture = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("builder preview"),
                size: wgpu::Extent3d { width: size.0, height: size.1, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: FORMAT,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[FORMAT.remove_srgb_suffix()],
            });
            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
            let egui_view =
                texture.create_view(&wgpu::TextureViewDescriptor { format: Some(FORMAT.remove_srgb_suffix()), ..Default::default() });
            let id = egui_renderer.register_native_texture(device, &egui_view, wgpu::FilterMode::Linear);
            self.target = Some(Target { _texture: texture, view, id, size });
        }
        let target = self.target.as_ref().expect("created above");

        let f = input.figure;
        let bounds = f.base.bounds();
        let height = bounds.max.y.max(f.ring_anchor.y + 10.0).max(40.0);
        let radius = (bounds.size().x.max(bounds.size().z) * 0.9).max(height * 0.62);
        let t = input.time;

        let (motion, offset, heading) = match input.anim {
            PreviewAnim::Working => (Some(f.animations.working.motion.clone()), Vec3::ZERO, 0.0),
            PreviewAnim::NeedsYou => (Some(f.animations.waiting.motion.clone()), Vec3::ZERO, 0.0),
            PreviewAnim::Resting => (Some(f.animations.resting.motion.clone()), Vec3::ZERO, 0.0),
            PreviewAnim::Hop => (Some(Motion::Hop { count: 1, height: height * 0.14, period: 1.1 }), Vec3::ZERO, 0.0),
            PreviewAnim::Walk => {
                let phase = t * TAU / 5.0;
                let x = phase.sin() * radius * 0.45;
                let moving_right = phase.cos() >= 0.0;
                let hop = (t * 5.5 * PI).sin().abs() * 3.0;
                (None, Vec3::new(x, hop, 0.0), if moving_right { PI / 2.0 } else { -PI / 2.0 })
            }
        };
        let mut m = Mat4::from_rotation_translation(Quat::from_rotation_y(input.yaw + heading), offset);
        if let Some(motion) = &motion {
            m *= motion_matrix(motion, t);
        }
        let resting = input.anim == PreviewAnim::Resting;
        let (brightness, emissive) = if resting { (0.7, 0.12) } else { (1.0, 1.0) };

        let mut draws = vec![
            Draw {
                mesh: self.platform,
                transform: Mat4::from_scale(Vec3::new(radius, 8.0, radius)) * Mat4::from_translation(Vec3::NEG_Y),
                brightness: 1.0,
                emissive_scale: 1.0,
            },
            Draw { mesh: *base, transform: m, brightness, emissive_scale: emissive },
        ];
        for (part, mesh) in f.param_parts.iter().zip(parts) {
            let def = f.params.iter().find(|p| p.name == part.param);
            if def.is_none_or(|d| param_value(d, input.params) != part.when) {
                continue;
            }
            let local = part.anim.as_ref().map_or(Mat4::IDENTITY, |motion| {
                Mat4::from_translation(part.pivot) * motion_matrix(motion, t) * Mat4::from_translation(-part.pivot)
            });
            draws.push(Draw { mesh: *mesh, transform: m * local, brightness, emissive_scale: emissive });
        }

        let ring_center = m.transform_point3(f.ring_anchor);
        let r = (bounds.size().x.max(bounds.size().z) * 0.35).clamp(10.0, 22.0);
        let ring = match input.anim {
            PreviewAnim::Working => Some((theme::linear(theme::WORKING), 1.0, t * 2.6, PI)),
            PreviewAnim::NeedsYou => Some((theme::linear(theme::NEEDS_YOU), 0.85 + 0.3 * (t * 3.5).sin().abs(), 0.0, TAU)),
            PreviewAnim::Resting => Some((theme::linear(theme::RESTING), 0.35, 0.0, TAU)),
            PreviewAnim::Walk | PreviewAnim::Hop => None,
        };
        let rings = ring
            .map(|(color, intensity, arc_start, arc_len)| Ring {
                center: ring_center,
                radius: r,
                width: 2.4,
                color,
                intensity,
                arc_start,
                arc_len,
            })
            .into_iter()
            .collect();
        let lift = m.transform_point3(Vec3::ZERO).y;
        let frame = Frame {
            camera: Camera {
                target: Vec3::new(0.0, height * 0.52, 0.0),
                azimuth_deg: 18.0,
                elevation_deg: 14.0,
                view_height: height * 1.75,
                aspect: size.0 as f32 / size.1 as f32,
            },
            clear_color: theme::linear(theme::PANEL),
            ambient_sky: [0.26, 0.28, 0.34],
            ambient_ground: [0.10, 0.09, 0.08],
            sun_direction: Vec3::new(-0.4, -0.9, -0.5).normalize(),
            sun_color: [1.0, 0.95, 0.88],
            sun_intensity: if resting { 0.45 } else { 0.75 },
            lights: vec![PointLight {
                position: Vec3::new(0.0, height * 2.4, height * 0.9),
                color: [1.0, 0.88, 0.69],
                intensity: if resting { 0.35 } else { 0.9 },
                radius: height * 6.0,
            }],
            draws,
            rings,
            shadows: vec![BlobShadow {
                center: Vec3::new(offset.x, 0.0, offset.z),
                radius: (radius * 0.45) * (1.0 - (lift / 60.0).clamp(0.0, 0.4)),
                opacity: 0.5,
            }],
            paths: Vec::new(),
        };
        self.renderer.render(device, queue, encoder, &target.view, size, &frame);
    }
}

fn param_value(def: &ParamDef, values: &HashMap<String, serde_json::Value>) -> serde_json::Value {
    if let Some(v) = values.get(&def.name) {
        return v.clone();
    }
    match &def.kind {
        ParamKind::Toggle { default } => (*default).into(),
        ParamKind::Choice { default, .. } => default.clone().into(),
        ParamKind::Number { default, .. } => (*default).into(),
        ParamKind::Count { default } => (*default).into(),
        ParamKind::Text { default } => default.clone().into(),
    }
}
