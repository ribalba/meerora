//! Render pipelines, the uniform block and the per-instance layout.

use std::mem::size_of;

use meerora_geom::Vertex;

use crate::MAX_POINT_LIGHTS;
use crate::overlay::OverlayVertex;

pub(crate) const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;

const COMMON_WGSL: &str = include_str!("shaders/common.wgsl");
const MESH_WGSL: &str = include_str!("shaders/mesh.wgsl");
const OVERLAY_WGSL: &str = include_str!("shaders/overlay.wgsl");

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, bytemuck::Pod, bytemuck::Zeroable)]
pub(crate) struct LightRaw {
    pub position_radius: [f32; 4],
    pub color: [f32; 4],
}

/// Mirrors `Globals` in common.wgsl.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, bytemuck::Pod, bytemuck::Zeroable)]
pub(crate) struct GlobalsRaw {
    pub view_proj: [[f32; 4]; 4],
    pub ambient_sky: [f32; 4],
    pub ambient_ground: [f32; 4],
    pub to_sun: [f32; 4],
    pub sun_color: [f32; 4],
    pub params: [f32; 4],
    pub lights: [LightRaw; MAX_POINT_LIGHTS],
}

/// Per-draw data, read as vertex attributes 4 to 11.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, bytemuck::Pod, bytemuck::Zeroable)]
pub(crate) struct InstanceRaw {
    pub model: [[f32; 4]; 4],
    /// Inverse transpose of the upper 3x3, one column per row, w unused.
    pub normal: [[f32; 4]; 3],
    /// x brightness, y emissive scale.
    pub params: [f32; 4],
}

const VERTEX_ATTRIBUTES: [wgpu::VertexAttribute; 4] = wgpu::vertex_attr_array![
    0 => Float32x3,
    1 => Float32x3,
    2 => Float32x3,
    3 => Float32,
];

const INSTANCE_ATTRIBUTES: [wgpu::VertexAttribute; 8] = wgpu::vertex_attr_array![
    4 => Float32x4,
    5 => Float32x4,
    6 => Float32x4,
    7 => Float32x4,
    8 => Float32x4,
    9 => Float32x4,
    10 => Float32x4,
    11 => Float32x4,
];

const OVERLAY_ATTRIBUTES: [wgpu::VertexAttribute; 5] = wgpu::vertex_attr_array![
    0 => Float32x3,
    1 => Float32x2,
    2 => Float32x4,
    3 => Float32x4,
    4 => Float32x4,
];

pub(crate) struct Pipelines {
    pub globals_layout: wgpu::BindGroupLayout,
    /// Meshes whose transform keeps the winding.
    pub mesh: wgpu::RenderPipeline,
    /// Meshes drawn with a mirroring transform, which flips the winding.
    pub mesh_mirrored: wgpu::RenderPipeline,
    pub shadow: wgpu::RenderPipeline,
    pub path: wgpu::RenderPipeline,
    pub ring: wgpu::RenderPipeline,
}

impl Pipelines {
    pub fn new(device: &wgpu::Device, color_format: wgpu::TextureFormat, sample_count: u32) -> Self {
        let globals_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("meerora globals"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: wgpu::BufferSize::new(size_of::<GlobalsRaw>() as u64),
                },
                count: None,
            }],
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("meerora"),
            bind_group_layouts: &[Some(&globals_layout)],
            immediate_size: 0,
        });

        let mesh_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("meerora mesh"),
            source: wgpu::ShaderSource::Wgsl(format!("{COMMON_WGSL}\n{MESH_WGSL}").into()),
        });
        let overlay_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("meerora overlay"),
            source: wgpu::ShaderSource::Wgsl(format!("{COMMON_WGSL}\n{OVERLAY_WGSL}").into()),
        });

        let multisample = wgpu::MultisampleState { count: sample_count, mask: !0, alpha_to_coverage_enabled: false };
        let mesh_buffers = [
            Some(wgpu::VertexBufferLayout {
                array_stride: size_of::<Vertex>() as u64,
                step_mode: wgpu::VertexStepMode::Vertex,
                attributes: &VERTEX_ATTRIBUTES,
            }),
            Some(wgpu::VertexBufferLayout {
                array_stride: size_of::<InstanceRaw>() as u64,
                step_mode: wgpu::VertexStepMode::Instance,
                attributes: &INSTANCE_ATTRIBUTES,
            }),
        ];
        let mesh_pipeline = |label: &str, front_face: wgpu::FrontFace| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(&layout),
                vertex: wgpu::VertexState {
                    module: &mesh_module,
                    entry_point: Some("vs_mesh"),
                    compilation_options: Default::default(),
                    buffers: &mesh_buffers,
                },
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleList,
                    front_face,
                    cull_mode: Some(wgpu::Face::Back),
                    ..Default::default()
                },
                depth_stencil: Some(wgpu::DepthStencilState {
                    format: DEPTH_FORMAT,
                    depth_write_enabled: Some(true),
                    depth_compare: Some(wgpu::CompareFunction::Less),
                    stencil: Default::default(),
                    bias: Default::default(),
                }),
                multisample,
                fragment: Some(wgpu::FragmentState {
                    module: &mesh_module,
                    entry_point: Some("fs_mesh"),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState { format: color_format, blend: None, write_mask: wgpu::ColorWrites::ALL })],
                }),
                multiview_mask: None,
                cache: None,
            })
        };

        let overlay_buffers = [Some(wgpu::VertexBufferLayout {
            array_stride: size_of::<OverlayVertex>() as u64,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &OVERLAY_ATTRIBUTES,
        })];
        let overlay_pipeline = |label: &str, entry: &str, blend: wgpu::BlendState| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(&layout),
                vertex: wgpu::VertexState {
                    module: &overlay_module,
                    entry_point: Some("vs_overlay"),
                    compilation_options: Default::default(),
                    buffers: &overlay_buffers,
                },
                primitive: wgpu::PrimitiveState { topology: wgpu::PrimitiveTopology::TriangleList, cull_mode: None, ..Default::default() },
                depth_stencil: Some(wgpu::DepthStencilState {
                    format: DEPTH_FORMAT,
                    depth_write_enabled: Some(false),
                    depth_compare: Some(wgpu::CompareFunction::LessEqual),
                    stencil: Default::default(),
                    bias: Default::default(),
                }),
                multisample,
                fragment: Some(wgpu::FragmentState {
                    module: &overlay_module,
                    entry_point: Some(entry),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: color_format,
                        blend: Some(blend),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                multiview_mask: None,
                cache: None,
            })
        };
        // Premultiplied colour. Alpha 0 adds light, alpha 1 covers the target.
        let premultiplied = wgpu::BlendState {
            color: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::One,
                dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                operation: wgpu::BlendOperation::Add,
            },
            alpha: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::Zero,
                dst_factor: wgpu::BlendFactor::One,
                operation: wgpu::BlendOperation::Add,
            },
        };

        Pipelines {
            mesh: mesh_pipeline("meerora mesh", wgpu::FrontFace::Ccw),
            mesh_mirrored: mesh_pipeline("meerora mesh mirrored", wgpu::FrontFace::Cw),
            shadow: overlay_pipeline("meerora shadows", "fs_shadow", wgpu::BlendState::ALPHA_BLENDING),
            path: overlay_pipeline("meerora paths", "fs_path", wgpu::BlendState::ALPHA_BLENDING),
            ring: overlay_pipeline("meerora rings", "fs_ring", premultiplied),
            globals_layout,
        }
    }
}
