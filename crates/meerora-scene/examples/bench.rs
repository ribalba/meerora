//! `cargo run --release -p meerora-scene --example bench` renders about 100k
//! triangles, 10 rings, shadows and a path at 1440x900 with 4x MSAA offscreen
//! and reports the frame time.
//!
//! Optional arguments: frame count (default 300) and triangle target
//! (default 100000).

use std::time::Instant;

use meerora_geom::{Mat4, Mesh, Vec3};
use meerora_scene::{BlobShadow, Camera, DashedPath, Draw, Frame, Gpu, IMAGE_FORMAT, PointLight, Renderer, RendererConfig, Ring};

const WIDTH: u32 = 1440;
const HEIGHT: u32 = 900;

/// Closed box, 12 triangles.
fn push_box(mesh: &mut Mesh, min: Vec3, max: Vec3, color: [f32; 3]) {
    let p = |x: f32, y: f32, z: f32| Vec3::new(x, y, z);
    let (a, b) = (min, max);
    let start = mesh.vertices.len();
    mesh.push_quad(p(a.x, b.y, b.z), p(b.x, b.y, b.z), p(b.x, b.y, a.z), p(a.x, b.y, a.z));
    mesh.push_quad(p(a.x, a.y, a.z), p(b.x, a.y, a.z), p(b.x, a.y, b.z), p(a.x, a.y, b.z));
    mesh.push_quad(p(b.x, a.y, b.z), p(b.x, a.y, a.z), p(b.x, b.y, a.z), p(b.x, b.y, b.z));
    mesh.push_quad(p(a.x, a.y, a.z), p(a.x, a.y, b.z), p(a.x, b.y, b.z), p(a.x, b.y, a.z));
    mesh.push_quad(p(a.x, a.y, b.z), p(b.x, a.y, b.z), p(b.x, b.y, b.z), p(a.x, b.y, b.z));
    mesh.push_quad(p(b.x, a.y, a.z), p(a.x, a.y, a.z), p(a.x, b.y, a.z), p(b.x, b.y, a.z));
    for v in &mut mesh.vertices[start..] {
        v.color = color;
    }
}

/// Deterministic pseudo random numbers in 0..1.
fn noise(i: u32) -> f32 {
    let mut x = i.wrapping_mul(0x9E37_79B9) ^ 0x85EB_CA6B;
    x ^= x >> 15;
    x = x.wrapping_mul(0x2C1B_3C6D);
    x ^= x >> 12;
    (x & 0xFFFF) as f32 / 65535.0
}

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let frames: u32 = args.next().map(|s| s.parse()).transpose()?.unwrap_or(300);
    let target_triangles: usize = args.next().map(|s| s.parse()).transpose()?.unwrap_or(100_000);

    let (gpu, info) = Gpu::headless_with_info()?;
    eprintln!("adapter: {} ({:?})", info.name, info.backend);
    let mut renderer = Renderer::new(&gpu.device, RendererConfig { color_format: IMAGE_FORMAT, sample_count: 4 });

    // 20 figures take a tenth of the budget, a floor of boxes the rest.
    let figure_boxes = (target_triangles / 10).div_ceil(12 * 20).max(1);
    let floor_boxes = target_triangles.saturating_sub(figure_boxes * 12 * 20).div_ceil(12);
    let mut space = Mesh::new();
    push_box(&mut space, Vec3::new(-20.0, -50.0, -20.0), Vec3::new(920.0, 0.0, 520.0), [0.02, 0.03, 0.04]);
    let side = (floor_boxes as f32).sqrt().ceil() as usize;
    let cell = 900.0 / side as f32;
    for i in 0..floor_boxes {
        let (x, z) = ((i % side) as f32 * cell, (i / side) as f32 * cell * 500.0 / 900.0);
        let h = 2.0 + noise(i as u32) * 30.0;
        push_box(
            &mut space,
            Vec3::new(x, 0.0, z),
            Vec3::new(x + cell * 0.9, h, z + cell * 0.5),
            [0.4, 0.3 + noise(i as u32 + 7) * 0.2, 0.2],
        );
    }
    let space_id = renderer.upload_mesh(&gpu.device, &space);
    let mut draws = vec![Draw { mesh: space_id, transform: Mat4::IDENTITY, brightness: 1.0, emissive_scale: 1.0 }];

    let mut figure = Mesh::new();
    for i in 0..figure_boxes {
        let y = i as f32 * 80.0 / figure_boxes as f32;
        push_box(&mut figure, Vec3::new(-15.0, y, -15.0), Vec3::new(15.0, y + 3.0, 15.0), [0.8, 0.7, 0.6]);
    }
    let mut rings = Vec::new();
    let mut shadows = Vec::new();
    for k in 0..20u32 {
        let id = renderer.upload_mesh(&gpu.device, &figure);
        let pos = Vec3::new(60.0 + noise(k * 3) * 780.0, 30.0, 40.0 + noise(k * 3 + 1) * 420.0);
        draws.push(Draw { mesh: id, transform: Mat4::from_translation(pos), brightness: 1.0, emissive_scale: 1.0 });
        shadows.push(BlobShadow { center: pos, radius: 40.0, opacity: 0.5 });
        if k < 10 {
            rings.push(Ring {
                center: pos + Vec3::Y * 95.0,
                radius: 24.0,
                width: 3.0,
                color: [0.9, 0.46, 0.1],
                intensity: 1.0,
                arc_start: k as f32,
                arc_len: if k % 2 == 0 { std::f32::consts::PI } else { std::f32::consts::TAU },
            });
        }
    }
    let lights = (0..8)
        .map(|i| PointLight {
            position: Vec3::new(100.0 + i as f32 * 100.0, 150.0, 250.0),
            color: [1.0, 0.8, 0.5],
            intensity: 0.3,
            radius: 400.0,
        })
        .collect();

    let mut frame = Frame {
        camera: Camera {
            target: Vec3::new(450.0, 60.0, 250.0),
            azimuth_deg: 45.0,
            elevation_deg: 30.0,
            view_height: 900.0,
            aspect: WIDTH as f32 / HEIGHT as f32,
        },
        clear_color: [0.004, 0.004, 0.006],
        ambient_sky: [0.3, 0.32, 0.4],
        ambient_ground: [0.1, 0.1, 0.1],
        sun_direction: Vec3::new(-0.35, -0.75, -0.55),
        sun_color: [1.0, 0.9, 0.8],
        sun_intensity: 0.5,
        lights,
        draws,
        rings,
        shadows,
        paths: vec![DashedPath {
            points: (0..20).map(|i| Vec3::new(50.0 + i as f32 * 40.0, 2.0, 250.0 + (i as f32).sin() * 80.0)).collect(),
            width: 5.0,
            dash: 12.0,
            gap: 8.0,
            color: [0.4, 0.3, 0.9],
            opacity: 0.9,
        }],
    };

    let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("bench target"),
        size: wgpu::Extent3d { width: WIDTH, height: HEIGHT, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: IMAGE_FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let view = texture.create_view(&Default::default());

    let mut render = |frame: &Frame| {
        let mut encoder = gpu.device.create_command_encoder(&Default::default());
        let stats = renderer.render(&gpu.device, &gpu.queue, &mut encoder, &view, (WIDTH, HEIGHT), frame);
        (gpu.queue.submit([encoder.finish()]), stats)
    };
    let wait = |index| gpu.device.poll(wgpu::PollType::Wait { submission_index: Some(index), timeout: None });

    // Warm up, then measure with the rings turning like the working state.
    let (index, stats) = render(&frame);
    wait(index)?;
    eprintln!("{} triangles, {} draw calls per frame", stats.triangles, stats.draw_calls);

    let mut cpu = 0.0;
    let mut slowest = 0.0f64;
    let start = Instant::now();
    for i in 0..frames {
        for (k, ring) in frame.rings.iter_mut().enumerate() {
            ring.arc_start = k as f32 + i as f32 * 0.05;
        }
        let t = Instant::now();
        let (index, _) = render(&frame);
        cpu += t.elapsed().as_secs_f64();
        // Waiting every frame measures the full GPU time of each frame, as a
        // window with vsync off would see it.
        wait(index)?;
        slowest = slowest.max(t.elapsed().as_secs_f64());
    }
    let total = start.elapsed().as_secs_f64();
    let per_frame = total / frames as f64;
    println!(
        "{frames} frames: {:.2} ms per frame ({:.0} fps), slowest {:.2} ms, CPU record {:.3} ms",
        per_frame * 1000.0,
        1.0 / per_frame,
        slowest * 1000.0,
        cpu / frames as f64 * 1000.0
    );
    Ok(())
}
