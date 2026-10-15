//! Headless rendering tests. Tests that need a GPU print a note and pass when
//! no adapter exists at all, not even a software one.

use std::f32::consts::{FRAC_PI_2, PI, TAU};
use std::sync::OnceLock;

use meerora_geom::{Mat4, Mesh, Vec2, Vec3, srgb_hex};
use meerora_scene::{
    BlobShadow, Camera, DashedPath, Draw, Frame, FrameStats, Gpu, IMAGE_FORMAT, Renderer, RendererConfig, Ring, render_to_image,
};

const W: u32 = 160;
const H: u32 = 120;

fn gpu() -> Option<&'static Gpu> {
    static GPU: OnceLock<Option<Gpu>> = OnceLock::new();
    GPU.get_or_init(|| match Gpu::headless() {
        Ok(gpu) => Some(gpu),
        Err(err) => {
            eprintln!("note: no GPU adapter, GPU tests are skipped: {err:#}");
            None
        }
    })
    .as_ref()
}

macro_rules! require_gpu {
    () => {
        match gpu() {
            Some(gpu) => gpu,
            None => {
                eprintln!("note: skipping, no adapter");
                return;
            }
        }
    };
}

fn camera(view_height: f32) -> Camera {
    Camera { target: Vec3::ZERO, azimuth_deg: 45.0, elevation_deg: 30.0, view_height, aspect: W as f32 / H as f32 }
}

fn empty_frame(camera: Camera) -> Frame {
    Frame {
        camera,
        clear_color: srgb_hex("#0B0D12").unwrap(),
        ambient_sky: [0.0; 3],
        ambient_ground: [0.0; 3],
        sun_direction: Vec3::new(-0.3, -1.0, -0.4),
        sun_color: [1.0; 3],
        sun_intensity: 0.0,
        lights: Vec::new(),
        draws: Vec::new(),
        rings: Vec::new(),
        shadows: Vec::new(),
        paths: Vec::new(),
    }
}

fn renderer(gpu: &Gpu, sample_count: u32) -> Renderer {
    Renderer::new(&gpu.device, RendererConfig { color_format: IMAGE_FORMAT, sample_count })
}

fn pixel(image: &image::RgbaImage, at: Vec2) -> [u8; 3] {
    let p = image.get_pixel(at.x.floor() as u32, at.y.floor() as u32).0;
    [p[0], p[1], p[2]]
}

fn srgb_bytes(hex: &str) -> [u8; 3] {
    let s = hex.trim_start_matches('#');
    [0, 1, 2].map(|i| u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).unwrap())
}

/// A square facing the camera, centred on the target.
fn facing_quad(camera: &Camera, half: f32, color: [f32; 3]) -> Mesh {
    let n = camera.toward_viewer();
    let right = Vec3::Y.cross(n).normalize();
    let up = n.cross(right);
    let c = camera.target;
    let mut mesh = Mesh::new();
    mesh.push_quad(c - right * half - up * half, c + right * half - up * half, c + right * half + up * half, c - right * half + up * half);
    for v in &mut mesh.vertices {
        v.color = color;
    }
    mesh
}

#[test]
fn clear_colour_comes_back_as_srgb() {
    let gpu = require_gpu!();
    for sample_count in [1, 4] {
        let mut frame = empty_frame(camera(100.0));
        frame.clear_color = srgb_hex("#7BBB3F").unwrap();
        let image = render_to_image(gpu, &mut renderer(gpu, sample_count), &frame, W, H).unwrap();
        for corner in [Vec2::ZERO, Vec2::new(W as f32 - 1.0, H as f32 - 1.0)] {
            let got = pixel(&image, corner);
            let want = srgb_bytes("#7BBB3F");
            for i in 0..3 {
                assert!(got[i].abs_diff(want[i]) <= 1, "sample count {sample_count}: corner {corner} is {got:?}, want {want:?}");
            }
        }
    }
}

#[test]
fn odd_widths_read_back_without_skew() {
    let gpu = require_gpu!();
    let cam = Camera { aspect: 77.0 / 33.0, ..camera(100.0) };
    let mut frame = empty_frame(cam);
    let mut r = renderer(gpu, 4);
    let quad = r.upload_mesh(&gpu.device, &facing_quad(&cam, 10.0, [1.0; 3]));
    frame.ambient_sky = [1.0; 3];
    frame.ambient_ground = [1.0; 3];
    frame.draws.push(Draw { mesh: quad, transform: Mat4::IDENTITY, brightness: 1.0, emissive_scale: 1.0 });
    let image = render_to_image(gpu, &mut r, &frame, 77, 33).unwrap();
    assert_eq!(image.dimensions(), (77, 33));
    let centre = pixel(&image, Vec2::new(38.5, 16.5));
    assert!(centre[0] > 200, "centre of the quad is {centre:?}");
    let corner = pixel(&image, Vec2::new(76.0, 0.0));
    assert!(corner[0] < 30, "corner is {corner:?}");
}

#[test]
fn lit_quad_is_brighter_than_background() {
    let gpu = require_gpu!();
    let cam = camera(100.0);
    let mut r = renderer(gpu, 4);
    let quad = r.upload_mesh(&gpu.device, &facing_quad(&cam, 20.0, [0.5, 0.5, 0.5]));
    let mut frame = empty_frame(cam);
    frame.ambient_sky = [0.4; 3];
    frame.ambient_ground = [0.2; 3];
    frame.sun_direction = -cam.toward_viewer();
    frame.sun_intensity = 0.6;
    frame.draws.push(Draw { mesh: quad, transform: Mat4::IDENTITY, brightness: 1.0, emissive_scale: 1.0 });
    let image = render_to_image(gpu, &mut r, &frame, W, H).unwrap();

    let centre = cam.project(Vec3::ZERO, Vec2::new(W as f32, H as f32)).unwrap();
    let lit = pixel(&image, centre);
    let background = pixel(&image, Vec2::new(2.0, 2.0));
    let luma = |p: [u8; 3]| p.iter().map(|&c| c as u32).sum::<u32>();
    assert!(luma(lit) > luma(background) + 150, "lit {lit:?} vs background {background:?}");

    // Seen from behind, the quad is culled and the background shows through.
    frame.draws[0].transform = Mat4::from_rotation_y(PI);
    let culled = render_to_image(gpu, &mut r, &frame, W, H).unwrap();
    assert_eq!(pixel(&culled, centre), background);
}

#[test]
fn ring_pixels_carry_the_ring_hue() {
    let gpu = require_gpu!();
    let cam = camera(60.0);
    let viewport = Vec2::new(W as f32, H as f32);
    let mut r = renderer(gpu, 4);
    for (hex, check) in [
        ("#8CD3FF", (|p: [u8; 3]| p[2] > p[1] && p[1] > p[0]) as fn([u8; 3]) -> bool),
        ("#F2B45A", |p: [u8; 3]| p[0] > p[1] && p[1] > p[2]),
        ("#9B8CF0", |p: [u8; 3]| p[2] > p[0] && p[0] > p[1]),
    ] {
        let mut frame = empty_frame(cam);
        frame.rings.push(Ring {
            center: Vec3::ZERO,
            radius: 20.0,
            width: 4.0,
            color: srgb_hex(hex).unwrap(),
            intensity: 1.0,
            arc_start: 0.0,
            arc_len: PI,
        });
        let image = render_to_image(gpu, &mut r, &frame, W, H).unwrap();
        let on_ring = |theta: f32| Vec3::new(20.0 * theta.cos(), 0.0, -20.0 * theta.sin());

        // The arc runs from angle 0 over pi, which is the half towards -z.
        let core = pixel(&image, cam.project(on_ring(FRAC_PI_2), viewport).unwrap());
        assert!(check(core), "{hex}: ring core is {core:?}");
        let want = srgb_bytes(hex);
        for i in 0..3 {
            assert!(core[i].abs_diff(want[i]) <= 24, "{hex}: ring core {core:?} is far from {want:?}");
        }
        let gap = pixel(&image, cam.project(on_ring(3.0 * FRAC_PI_2), viewport).unwrap());
        let background = pixel(&image, Vec2::new(2.0, 2.0));
        assert_eq!(gap, background, "{hex}: the open half of the arc must stay empty");
    }
}

#[test]
fn project_and_ray_agree() {
    let viewport = Vec2::new(1440.0, 900.0);
    for (azimuth_deg, elevation_deg) in [(45.0, 30.0), (10.0, 60.0), (200.0, 15.0)] {
        let cam = Camera { target: Vec3::new(450.0, 40.0, 250.0), azimuth_deg, elevation_deg, view_height: 1000.0, aspect: 1.6 };
        let centre = cam.project(cam.target, viewport).unwrap();
        assert!((centre - viewport * 0.5).length() < 1e-2, "target projects to {centre}");
        for p in [Vec3::new(0.0, 0.0, 0.0), Vec3::new(900.0, 300.0, 500.0), Vec3::new(120.0, 80.0, 430.0), Vec3::new(700.0, -50.0, 10.0)] {
            let pixel = cam.project(p, viewport).expect("inside the depth range");
            let (origin, dir) = cam.ray(pixel, viewport);
            assert!((dir.length() - 1.0).abs() < 1e-5);
            let along = (p - origin).dot(dir);
            assert!(along > 0.0, "the point lies in front of the ray origin");
            let miss = (p - (origin + dir * along)).length();
            assert!(miss < 0.05, "{p} misses the ray through its pixel by {miss} cm");
        }
    }

    let cam = Camera { target: Vec3::ZERO, azimuth_deg: 45.0, elevation_deg: 30.0, view_height: 100.0, aspect: 1.0 };
    let v = cam.toward_viewer();
    assert!(v.x > 0.0 && v.y > 0.0 && v.z > 0.0 && (v.x - v.z).abs() < 1e-6);
    assert!((v.y - 0.5).abs() < 1e-6);
    assert!(cam.project(-v * 6000.0, Vec2::splat(100.0)).is_none(), "beyond the far plane");
}

#[test]
fn remove_mesh_then_render_does_not_panic() {
    let gpu = require_gpu!();
    let cam = camera(100.0);
    let mut r = renderer(gpu, 4);
    let quad = r.upload_mesh(&gpu.device, &facing_quad(&cam, 20.0, [0.8; 3]));
    let empty = r.upload_mesh(&gpu.device, &Mesh::new());
    let mut frame = empty_frame(cam);
    let draw = |mesh| Draw { mesh, transform: Mat4::IDENTITY, brightness: 1.0, emissive_scale: 1.0 };
    frame.draws = vec![draw(quad), draw(empty), draw(quad)];
    render_to_image(gpu, &mut r, &frame, W, H).unwrap();
    r.remove_mesh(quad);
    r.remove_mesh(quad);
    let image = render_to_image(gpu, &mut r, &frame, W, H).unwrap();
    let centre = cam.project(Vec3::ZERO, Vec2::new(W as f32, H as f32)).unwrap();
    assert_eq!(pixel(&image, centre), pixel(&image, Vec2::new(2.0, 2.0)));
}

#[test]
fn stats_count_batches_and_overlays() {
    let gpu = require_gpu!();
    let cam = camera(100.0);
    let mut r = renderer(gpu, 1);
    let quad = r.upload_mesh(&gpu.device, &facing_quad(&cam, 5.0, [0.8; 3]));
    let mut frame = empty_frame(cam);
    let draw = |x: f32, mirror: f32| Draw {
        mesh: quad,
        transform: Mat4::from_translation(Vec3::new(x, 0.0, 0.0)) * Mat4::from_scale(Vec3::new(mirror, 1.0, 1.0)),
        brightness: 1.0,
        emissive_scale: 1.0,
    };
    frame.draws = vec![draw(-20.0, 1.0), draw(0.0, 1.0), draw(20.0, -1.0)];
    frame.shadows.push(BlobShadow { center: Vec3::ZERO, radius: 10.0, opacity: 0.5 });
    frame.paths.push(DashedPath {
        points: vec![Vec3::ZERO, Vec3::new(30.0, 0.0, 0.0)],
        width: 2.0,
        dash: 4.0,
        gap: 3.0,
        color: [1.0; 3],
        opacity: 1.0,
    });
    frame.rings.push(Ring {
        center: Vec3::Y * 10.0,
        radius: 8.0,
        width: 1.0,
        color: [1.0; 3],
        intensity: 1.0,
        arc_start: 0.0,
        arc_len: TAU,
    });

    let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: None,
        size: wgpu::Extent3d { width: W, height: H, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: IMAGE_FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let view = texture.create_view(&Default::default());
    let mut encoder = gpu.device.create_command_encoder(&Default::default());
    let stats = r.render(&gpu.device, &gpu.queue, &mut encoder, &view, (W, H), &frame);
    gpu.queue.submit([encoder.finish()]);

    // Two batches for the quad (plain and mirrored) plus one call per overlay kind.
    assert_eq!(stats.draw_calls, 5);
    let overlay_triangles = stats.triangles - 3 * 2;
    assert!(overlay_triangles > 2 + 2, "{stats:?}");
    assert_eq!(
        r.render(&gpu.device, &gpu.queue, &mut gpu.device.create_command_encoder(&Default::default()), &view, (0, 0), &frame),
        FrameStats::default()
    );
}

/// Devices open and close on some threads while others draw on the shared
/// device. With VK_EXT_debug_utils on, the Vulkan loader crashed here now
/// and then (`Gpu::headless_with_info`).
#[test]
fn devices_open_and_close_while_others_draw() {
    let shared = require_gpu!();
    std::thread::scope(|s| {
        for _ in 0..3 {
            s.spawn(|| {
                for _ in 0..4 {
                    let gpu = Gpu::headless().unwrap();
                    render_to_image(&gpu, &mut renderer(&gpu, 1), &empty_frame(camera(100.0)), 16, 16).unwrap();
                }
            });
        }
        for _ in 0..3 {
            s.spawn(|| {
                let mut r = renderer(shared, 1);
                for _ in 0..30 {
                    render_to_image(shared, &mut r, &empty_frame(camera(100.0)), 16, 16).unwrap();
                }
            });
        }
    });
}
