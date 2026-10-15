//! wgpu renderer for the meerora space (D1, D2, D3).
//!
//! The renderer knows nothing about windows. It draws a [`Frame`] into any
//! texture view it is given, so the app can hand it a window surface today and
//! a compositor output later (D15), and tests can render headless.
//!
//! Look: flat-shaded low-poly meshes, lit from inside by warm point lights,
//! a hemisphere ambient and a soft sun, emissive parts that glow, a dark
//! surround. Rings, blob shadows and dashed paths are drawn by the renderer
//! itself and need no meshes.
//!
//! Light is additive and unitless. A surface receiving a total of 1 from the
//! ambient, the sun and the point lights shows its base colour unchanged, so a
//! frame whose lights add up to about 1 keeps the palette of the mockups. A
//! gentle shoulder above 0.8 rolls off anything brighter instead of clipping.

mod overlay;
mod pipelines;

use std::collections::HashMap;
use std::ops::Range;

use anyhow::Context as _;
use meerora_geom::{Mat3, Mat4, Mesh, Vec2, Vec3};
use wgpu::util::DeviceExt as _;

use overlay::OverlayVertex;
use pipelines::{DEPTH_FORMAT, GlobalsRaw, InstanceRaw, LightRaw, Pipelines};

#[derive(Clone, Copy, Debug)]
pub struct RendererConfig {
    pub color_format: wgpu::TextureFormat,
    /// 1 or 4.
    pub sample_count: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MeshId(pub u32);

/// Fixed orthographic camera looking down at the diorama.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Camera {
    pub target: Vec3,
    /// Degrees around y. 45 looks along -x -z from the +x +z corner.
    pub azimuth_deg: f32,
    /// Degrees above the horizon, about 30 for the mockup look.
    pub elevation_deg: f32,
    /// Visible world height in cm.
    pub view_height: f32,
    /// Viewport width / height.
    pub aspect: f32,
}

/// Distance from the eye to [`Camera::target`] in cm.
const EYE_DISTANCE: f32 = 5000.0;
const NEAR: f32 = 10.0;
const FAR: f32 = 2.0 * EYE_DISTANCE;

impl Camera {
    pub fn view_proj(&self) -> Mat4 {
        let eye = self.target + self.toward_viewer() * EYE_DISTANCE;
        let view = glam::camera::rh::view::look_at_mat4(eye, self.target, Vec3::Y);
        let half_h = self.view_height.max(1e-3) * 0.5;
        let half_w = half_h * self.aspect.max(1e-3);
        let proj = glam::camera::rh::proj::directx::orthographic(-half_w, half_w, -half_h, half_h, NEAR, FAR);
        proj * view
    }

    /// World point to pixel position (origin top left), None if outside the
    /// depth range.
    pub fn project(&self, world: Vec3, viewport: Vec2) -> Option<Vec2> {
        let clip = self.view_proj() * world.extend(1.0);
        if clip.w.abs() <= f32::EPSILON {
            return None;
        }
        let ndc = clip.truncate() / clip.w;
        if !(0.0..=1.0).contains(&ndc.z) {
            return None;
        }
        Some(Vec2::new((ndc.x + 1.0) * 0.5 * viewport.x, (1.0 - ndc.y) * 0.5 * viewport.y))
    }

    /// Picking ray through a pixel: (origin, normalized direction).
    pub fn ray(&self, pixel: Vec2, viewport: Vec2) -> (Vec3, Vec3) {
        let ndc = Vec2::new(pixel.x / viewport.x.max(1e-3) * 2.0 - 1.0, 1.0 - pixel.y / viewport.y.max(1e-3) * 2.0);
        let origin = self.view_proj().inverse().project_point3(ndc.extend(0.0));
        (origin, -self.toward_viewer())
    }

    /// Unit vector pointing from the scene towards the camera.
    pub fn toward_viewer(&self) -> Vec3 {
        // Straight up or down has no defined screen up direction.
        let elevation = self.elevation_deg.clamp(-89.9, 89.9).to_radians();
        let azimuth = self.azimuth_deg.to_radians();
        Vec3::new(elevation.cos() * azimuth.sin(), elevation.sin(), elevation.cos() * azimuth.cos())
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PointLight {
    pub position: Vec3,
    /// Linear RGB.
    pub color: [f32; 3],
    pub intensity: f32,
    /// Distance where the light reaches zero.
    pub radius: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Draw {
    pub mesh: MeshId,
    pub transform: Mat4,
    /// Multiplies lit colour. 1 is normal, about 0.6 for a resting figure.
    pub brightness: f32,
    /// Multiplies vertex emission. 0 switches eyes and screens off.
    pub emissive_scale: f32,
}

/// Status ring (D3): a glowing flat band in the xz plane. A partial arc is
/// drawn from `arc_start` over `arc_len` radians, counter-clockwise seen from
/// above. `arc_len >= TAU` draws the full ring.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Ring {
    pub center: Vec3,
    pub radius: f32,
    pub width: f32,
    /// Linear RGB.
    pub color: [f32; 3],
    /// 1 is the normal glow. Values around 0.3 read as a dim ring.
    pub intensity: f32,
    pub arc_start: f32,
    pub arc_len: f32,
}

/// Soft dark ellipse on the ground under a figure.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BlobShadow {
    pub center: Vec3,
    pub radius: f32,
    pub opacity: f32,
}

/// Dashed line on the ground, used for the path an agent walked.
#[derive(Clone, Debug, PartialEq)]
pub struct DashedPath {
    pub points: Vec<Vec3>,
    pub width: f32,
    pub dash: f32,
    pub gap: f32,
    /// Linear RGB.
    pub color: [f32; 3],
    pub opacity: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Frame {
    pub camera: Camera,
    /// Linear RGB.
    pub clear_color: [f32; 3],
    pub ambient_sky: [f32; 3],
    pub ambient_ground: [f32; 3],
    /// Direction the sunlight travels (pointing from the sun into the scene).
    pub sun_direction: Vec3,
    pub sun_color: [f32; 3],
    pub sun_intensity: f32,
    /// At most [`MAX_POINT_LIGHTS`] are used.
    pub lights: Vec<PointLight>,
    pub draws: Vec<Draw>,
    pub rings: Vec<Ring>,
    pub shadows: Vec<BlobShadow>,
    pub paths: Vec<DashedPath>,
}

pub const MAX_POINT_LIGHTS: usize = 16;

/// Statistics of the last rendered frame, for the F3 overlay (D2).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct FrameStats {
    pub triangles: usize,
    pub draw_calls: usize,
}

/// Mesh buffers on the GPU. Empty meshes keep an entry without buffers.
struct GpuMesh {
    vertices: wgpu::Buffer,
    indices: wgpu::Buffer,
    index_count: u32,
}

/// Consecutive instances of one mesh, drawn with one call.
struct Batch {
    mesh: u32,
    mirrored: bool,
    instances: Range<u32>,
}

/// Size-dependent attachments, recreated when the target size changes.
struct Targets {
    size: (u32, u32),
    msaa: Option<wgpu::TextureView>,
    depth: wgpu::TextureView,
}

/// Vertex buffer rewritten every frame that grows to the largest frame seen.
struct DynamicBuffer {
    label: &'static str,
    buffer: wgpu::Buffer,
}

impl DynamicBuffer {
    fn new(device: &wgpu::Device, label: &'static str, size: u64) -> Self {
        Self { label, buffer: Self::create(device, label, size) }
    }

    fn create(device: &wgpu::Device, label: &'static str, size: u64) -> wgpu::Buffer {
        device.create_buffer(&wgpu::BufferDescriptor {
            label: Some(label),
            size,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        })
    }

    fn write(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, data: &[u8]) {
        let len = data.len() as u64;
        if len > self.buffer.size() {
            self.buffer = Self::create(device, self.label, len.next_power_of_two());
        }
        queue.write_buffer(&self.buffer, 0, data);
    }
}

pub struct Renderer {
    config: RendererConfig,
    pipelines: Pipelines,
    globals: wgpu::Buffer,
    globals_bind_group: wgpu::BindGroup,
    meshes: HashMap<u32, Option<GpuMesh>>,
    next_mesh: u32,
    instances: DynamicBuffer,
    overlay: DynamicBuffer,
    targets: Option<Targets>,
    // Scratch space reused every frame.
    order: Vec<(u32, bool, usize)>,
    instance_data: Vec<InstanceRaw>,
    batches: Vec<Batch>,
    overlay_data: Vec<OverlayVertex>,
}

impl Renderer {
    /// A `sample_count` of 4 or more selects 4x MSAA, anything else renders
    /// without multisampling.
    pub fn new(device: &wgpu::Device, config: RendererConfig) -> Self {
        let config = RendererConfig { sample_count: if config.sample_count >= 4 { 4 } else { 1 }, ..config };
        let pipelines = Pipelines::new(device, config.color_format, config.sample_count);
        let globals = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("meerora globals"),
            size: size_of::<GlobalsRaw>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let globals_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("meerora globals"),
            layout: &pipelines.globals_layout,
            entries: &[wgpu::BindGroupEntry { binding: 0, resource: globals.as_entire_binding() }],
        });
        Renderer {
            config,
            pipelines,
            globals,
            globals_bind_group,
            meshes: HashMap::new(),
            next_mesh: 0,
            instances: DynamicBuffer::new(device, "meerora instances", 256 * size_of::<InstanceRaw>() as u64),
            overlay: DynamicBuffer::new(device, "meerora overlays", 4096 * size_of::<OverlayVertex>() as u64),
            targets: None,
            order: Vec::new(),
            instance_data: Vec::new(),
            batches: Vec::new(),
            overlay_data: Vec::new(),
        }
    }

    pub fn upload_mesh(&mut self, device: &wgpu::Device, mesh: &Mesh) -> MeshId {
        let id = self.next_mesh;
        self.next_mesh = self.next_mesh.wrapping_add(1);
        let gpu = (!mesh.is_empty()).then(|| GpuMesh {
            vertices: device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("meerora mesh vertices"),
                contents: bytemuck::cast_slice(&mesh.vertices),
                usage: wgpu::BufferUsages::VERTEX,
            }),
            indices: device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("meerora mesh indices"),
                contents: bytemuck::cast_slice(&mesh.indices),
                usage: wgpu::BufferUsages::INDEX,
            }),
            index_count: mesh.indices.len() as u32,
        });
        self.meshes.insert(id, gpu);
        MeshId(id)
    }

    /// Frees the mesh. Draws that still name it are skipped.
    pub fn remove_mesh(&mut self, id: MeshId) {
        self.meshes.remove(&id.0);
    }

    /// Records the frame into `encoder`. `target` is a single-sample view of
    /// `size` pixels in the configured colour format. With MSAA the renderer
    /// owns the multisampled and depth textures and resolves into `target`.
    ///
    /// Per-frame data goes through `queue.write_buffer`, so submit the encoder
    /// before rendering the next frame.
    pub fn render(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        target: &wgpu::TextureView,
        size: (u32, u32),
        frame: &Frame,
    ) -> FrameStats {
        if size.0 == 0 || size.1 == 0 {
            return FrameStats::default();
        }
        self.ensure_targets(device, size);
        queue.write_buffer(&self.globals, 0, bytemuck::bytes_of(&self.globals_raw(frame)));

        self.prepare_instances(frame);
        if !self.instance_data.is_empty() {
            self.instances.write(device, queue, bytemuck::cast_slice(&self.instance_data));
        }

        self.overlay_data.clear();
        frame.shadows.iter().for_each(|s| overlay::push_shadow(&mut self.overlay_data, s));
        let shadows = 0..self.overlay_data.len() as u32;
        frame.paths.iter().for_each(|p| overlay::push_path(&mut self.overlay_data, p));
        let paths = shadows.end..self.overlay_data.len() as u32;
        frame.rings.iter().for_each(|r| overlay::push_ring(&mut self.overlay_data, r));
        let rings = paths.end..self.overlay_data.len() as u32;
        if !self.overlay_data.is_empty() {
            self.overlay.write(device, queue, bytemuck::cast_slice(&self.overlay_data));
        }

        let [r, g, b] = if self.config.color_format.is_srgb() { frame.clear_color } else { frame.clear_color.map(linear_to_srgb) };
        let clear = wgpu::Color { r: r as f64, g: g as f64, b: b as f64, a: 1.0 };
        let targets = self.targets.as_ref().expect("targets exist after ensure_targets");
        let (view, resolve_target, store) = match &targets.msaa {
            Some(msaa) => (msaa, Some(target), wgpu::StoreOp::Discard),
            None => (target, None, wgpu::StoreOp::Store),
        };
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("meerora scene"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view,
                depth_slice: None,
                resolve_target,
                ops: wgpu::Operations { load: wgpu::LoadOp::Clear(clear), store },
            })],
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: &targets.depth,
                depth_ops: Some(wgpu::Operations { load: wgpu::LoadOp::Clear(1.0), store: wgpu::StoreOp::Discard }),
                stencil_ops: None,
            }),
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_bind_group(0, &self.globals_bind_group, &[]);

        let mut stats = FrameStats::default();
        if !self.batches.is_empty() {
            pass.set_vertex_buffer(1, self.instances.buffer.slice(..));
        }
        let mut mirrored_bound = None;
        for batch in &self.batches {
            let Some(Some(mesh)) = self.meshes.get(&batch.mesh) else { continue };
            if mirrored_bound != Some(batch.mirrored) {
                pass.set_pipeline(if batch.mirrored { &self.pipelines.mesh_mirrored } else { &self.pipelines.mesh });
                mirrored_bound = Some(batch.mirrored);
            }
            pass.set_vertex_buffer(0, mesh.vertices.slice(..));
            pass.set_index_buffer(mesh.indices.slice(..), wgpu::IndexFormat::Uint32);
            pass.draw_indexed(0..mesh.index_count, 0, batch.instances.clone());
            stats.triangles += (mesh.index_count / 3) as usize * batch.instances.len();
            stats.draw_calls += 1;
        }

        if !self.overlay_data.is_empty() {
            pass.set_vertex_buffer(0, self.overlay.buffer.slice(..));
        }
        for (pipeline, range) in [(&self.pipelines.shadow, shadows), (&self.pipelines.path, paths), (&self.pipelines.ring, rings)] {
            if range.is_empty() {
                continue;
            }
            pass.set_pipeline(pipeline);
            stats.triangles += range.len() / 3;
            stats.draw_calls += 1;
            pass.draw(range, 0..1);
        }
        stats
    }

    fn ensure_targets(&mut self, device: &wgpu::Device, size: (u32, u32)) {
        if self.targets.as_ref().is_some_and(|t| t.size == size) {
            return;
        }
        let texture = |label: &str, format: wgpu::TextureFormat| {
            device
                .create_texture(&wgpu::TextureDescriptor {
                    label: Some(label),
                    size: wgpu::Extent3d { width: size.0, height: size.1, depth_or_array_layers: 1 },
                    mip_level_count: 1,
                    sample_count: self.config.sample_count,
                    dimension: wgpu::TextureDimension::D2,
                    format,
                    usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                    view_formats: &[],
                })
                .create_view(&wgpu::TextureViewDescriptor::default())
        };
        let msaa = (self.config.sample_count > 1).then(|| texture("meerora msaa colour", self.config.color_format));
        let depth = texture("meerora depth", DEPTH_FORMAT);
        self.targets = Some(Targets { size, msaa, depth });
    }

    fn globals_raw(&self, frame: &Frame) -> GlobalsRaw {
        let mut lights = [LightRaw::default(); MAX_POINT_LIGHTS];
        let mut count = 0;
        for l in frame.lights.iter().filter(|l| l.radius > 0.0 && l.intensity > 0.0).take(MAX_POINT_LIGHTS) {
            lights[count] =
                LightRaw { position_radius: l.position.extend(l.radius).to_array(), color: rgb4(l.color.map(|c| c * l.intensity)) };
            count += 1;
        }
        GlobalsRaw {
            view_proj: frame.camera.view_proj().to_cols_array_2d(),
            ambient_sky: rgb4(frame.ambient_sky),
            ambient_ground: rgb4(frame.ambient_ground),
            to_sun: (-frame.sun_direction).normalize_or_zero().extend(0.0).to_array(),
            sun_color: rgb4(frame.sun_color.map(|c| c * frame.sun_intensity.max(0.0))),
            params: [count as f32, if self.config.color_format.is_srgb() { 0.0 } else { 1.0 }, 0.0, 0.0],
            lights,
        }
    }

    /// Sorts the draws by mesh and winding and fills the instance list and
    /// the batches.
    fn prepare_instances(&mut self, frame: &Frame) {
        self.order.clear();
        for (i, draw) in frame.draws.iter().enumerate() {
            if matches!(self.meshes.get(&draw.mesh.0), Some(Some(_))) {
                let mirrored = Mat3::from_mat4(draw.transform).determinant() < 0.0;
                self.order.push((draw.mesh.0, mirrored, i));
            }
        }
        self.order.sort_unstable();

        self.instance_data.clear();
        self.batches.clear();
        for &(mesh, mirrored, i) in &self.order {
            self.instance_data.push(instance_raw(&frame.draws[i]));
            let end = self.instance_data.len() as u32;
            match self.batches.last_mut() {
                Some(b) if b.mesh == mesh && b.mirrored == mirrored => b.instances.end = end,
                _ => self.batches.push(Batch { mesh, mirrored, instances: end - 1..end }),
            }
        }
    }
}

fn instance_raw(draw: &Draw) -> InstanceRaw {
    let m = Mat3::from_mat4(draw.transform);
    let n = if m.determinant().abs() > 1e-12 { m.inverse().transpose() } else { m };
    InstanceRaw {
        model: draw.transform.to_cols_array_2d(),
        normal: [n.x_axis, n.y_axis, n.z_axis].map(|c| c.extend(0.0).to_array()),
        params: [draw.brightness, draw.emissive_scale, 0.0, 0.0],
    }
}

fn rgb4(c: [f32; 3]) -> [f32; 4] {
    [c[0], c[1], c[2], 0.0]
}

fn linear_to_srgb(c: f32) -> f32 {
    if c <= 0.003_130_8 { c * 12.92 } else { 1.055 * c.powf(1.0 / 2.4) - 0.055 }
}

/// A device without a window, for snapshots and tests. Falls back to a
/// software adapter (llvmpipe) when no GPU is available.
pub struct Gpu {
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
}

impl Gpu {
    pub fn headless() -> anyhow::Result<Gpu> {
        Self::headless_with_info().map(|(gpu, _)| gpu)
    }

    /// Like [`Gpu::headless`], also returns which adapter was picked. Vulkan
    /// and GL are tried, `WGPU_BACKEND` overrides that choice.
    pub fn headless_with_info() -> anyhow::Result<(Gpu, wgpu::AdapterInfo)> {
        let instance = wgpu::Instance::new(
            wgpu::InstanceDescriptor {
                backends: wgpu::Backends::VULKAN | wgpu::Backends::GL,
                // Headless devices name no objects. Tests open several devices at
                // once, and the Vulkan loader crashes (SIGSEGV in
                // loader_get_icd_and_device) when one thread names an object while
                // another creates or drops a device. Discarding labels is not
                // enough, because wgpu-hal still names every command encoder and
                // some semaphores whenever VK_EXT_debug_utils is on, and DEBUG,
                // part of the default flags of debug builds, turns it on. Without
                // DEBUG nothing is named. WGPU_DEBUG=1 brings the names back.
                flags: (wgpu::InstanceFlags::default() | wgpu::InstanceFlags::DISCARD_HAL_LABELS) - wgpu::InstanceFlags::DEBUG,
                ..wgpu::InstanceDescriptor::new_without_display_handle()
            }
            .with_env(),
        );
        let request = |force_fallback_adapter| {
            pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                force_fallback_adapter,
                compatible_surface: None,
                apply_limit_buckets: false,
            }))
        };
        let adapter = request(false).or_else(|_| request(true)).context("no Vulkan, GL or software adapter available")?;
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("meerora headless"),
            required_limits: adapter.limits(),
            ..Default::default()
        }))
        .context("could not open the adapter")?;
        Ok((Gpu { device, queue }, adapter.get_info()))
    }
}

/// Colour format used by [`render_to_image`].
pub const IMAGE_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

/// Renders one frame and reads it back. `renderer` must have been created with
/// [`IMAGE_FORMAT`].
pub fn render_to_image(gpu: &Gpu, renderer: &mut Renderer, frame: &Frame, width: u32, height: u32) -> anyhow::Result<image::RgbaImage> {
    anyhow::ensure!(
        renderer.config.color_format == IMAGE_FORMAT,
        "the renderer draws {:?}, render_to_image needs {IMAGE_FORMAT:?}",
        renderer.config.color_format
    );
    anyhow::ensure!(width > 0 && height > 0, "image size {width}x{height} is empty");
    let device = &gpu.device;
    let extent = wgpu::Extent3d { width, height, depth_or_array_layers: 1 };
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("meerora snapshot"),
        size: extent,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: IMAGE_FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());

    // Rows of a texture copy must start on 256-byte boundaries.
    let row_bytes = width * 4;
    let padded_row_bytes = row_bytes.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let readback = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("meerora snapshot readback"),
        size: padded_row_bytes as u64 * height as u64,
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });

    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("meerora snapshot") });
    renderer.render(device, &gpu.queue, &mut encoder, &view, (width, height), frame);
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo { texture: &texture, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
        wgpu::TexelCopyBufferInfo {
            buffer: &readback,
            layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(padded_row_bytes), rows_per_image: Some(height) },
        },
        extent,
    );
    let submission = gpu.queue.submit([encoder.finish()]);

    let (sender, receiver) = std::sync::mpsc::channel();
    readback.map_async(wgpu::MapMode::Read, .., move |result| {
        let _ = sender.send(result);
    });
    device.poll(wgpu::PollType::Wait { submission_index: Some(submission), timeout: None }).context("waiting for the GPU failed")?;
    receiver.recv().context("the readback was dropped")?.context("mapping the readback buffer failed")?;

    let mut pixels = Vec::with_capacity(row_bytes as usize * height as usize);
    {
        let data = readback.get_mapped_range(..).context("reading the readback buffer failed")?;
        for row in data.chunks_exact(padded_row_bytes as usize) {
            pixels.extend_from_slice(&row[..row_bytes as usize]);
        }
    }
    readback.unmap();
    image::RgbaImage::from_raw(width, height, pixels).context("readback size does not match the image")
}
