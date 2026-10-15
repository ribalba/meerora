//! Small rendered pictures of each figure for the sidebar, the rail and the
//! agent panel. Rendered once per figure build with their own renderer.

use std::collections::HashMap;

use meerora_geom::{Mat4, Vec3};
use meerora_scene::{Camera, Draw, Frame, MeshId, PointLight, Renderer, RendererConfig};
use meerora_script::FigureDesc;

use crate::theme;

const SIZE: u32 = 192;
const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

struct Portrait {
    meshes: Vec<MeshId>,
    _texture: wgpu::Texture,
    id: egui::TextureId,
}

pub struct Portraits {
    renderer: Renderer,
    items: HashMap<String, Portrait>,
}

impl Portraits {
    pub fn new(device: &wgpu::Device) -> Self {
        Self { renderer: Renderer::new(device, RendererConfig { color_format: FORMAT, sample_count: 4 }), items: HashMap::new() }
    }

    pub fn get(&self, agent: &str) -> Option<egui::TextureId> {
        self.items.get(agent).map(|p| p.id)
    }

    pub fn remove(&mut self, agent: &str, egui_renderer: &mut egui_wgpu::Renderer) {
        if let Some(old) = self.items.remove(agent) {
            for m in old.meshes {
                self.renderer.remove_mesh(m);
            }
            egui_renderer.free_texture(&old.id);
        }
    }

    pub fn update(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        egui_renderer: &mut egui_wgpu::Renderer,
        agent: &str,
        figure: &FigureDesc,
    ) {
        self.remove(agent, egui_renderer);
        let base = self.renderer.upload_mesh(device, &figure.base);
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("portrait"),
            size: wgpu::Extent3d { width: SIZE, height: SIZE, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[FORMAT.remove_srgb_suffix()],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        // egui works in gamma space and samples the encoded bytes through a non-sRGB view.
        let egui_view =
            texture.create_view(&wgpu::TextureViewDescriptor { format: Some(FORMAT.remove_srgb_suffix()), ..Default::default() });

        let b = figure.base.bounds();
        let size = b.size();
        let target = b.center();
        let frame = Frame {
            camera: Camera {
                target,
                azimuth_deg: 28.0,
                elevation_deg: 14.0,
                view_height: size.y.max(size.x).max(size.z) * 1.22,
                aspect: 1.0,
            },
            clear_color: theme::linear([0x16, 0x1A, 0x23]),
            ambient_sky: [0.30, 0.32, 0.38],
            ambient_ground: [0.12, 0.10, 0.09],
            sun_direction: Vec3::new(-0.5, -0.8, -0.6).normalize(),
            sun_color: [1.0, 0.95, 0.88],
            sun_intensity: 0.9,
            lights: vec![PointLight {
                position: target + Vec3::new(80.0, 90.0, 120.0),
                color: [1.0, 0.86, 0.66],
                intensity: 0.8,
                radius: 420.0,
            }],
            draws: vec![Draw { mesh: base, transform: Mat4::IDENTITY, brightness: 1.0, emissive_scale: 1.0 }],
            rings: Vec::new(),
            shadows: Vec::new(),
            paths: Vec::new(),
        };
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("portrait") });
        self.renderer.render(device, queue, &mut encoder, &view, (SIZE, SIZE), &frame);
        queue.submit([encoder.finish()]);
        let id = egui_renderer.register_native_texture(device, &egui_view, wgpu::FilterMode::Linear);
        self.items.insert(agent.to_string(), Portrait { meshes: vec![base], _texture: texture, id });
    }
}
