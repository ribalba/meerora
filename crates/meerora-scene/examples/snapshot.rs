//! `cargo run -p meerora-scene --example snapshot -- out.png` renders a test
//! scene headless. Without an argument it writes
//! `target/snapshots/scene-test.png`.
//!
//! The scene follows the space mockup (docs/design/screenshots/1-space.png):
//! a cutaway room on a dark slab opening onto a garden, five figure stand-ins
//! with blob shadows, the four ring states of D3 and a dashed path. The
//! geometry is built from boxes so the example does not depend on
//! `meerora_geom::prim`.

use std::f32::consts::{PI, TAU};
use std::path::PathBuf;

use anyhow::Context as _;
use meerora_geom::{Mat4, Mesh, Vec3, srgb_hex};
use meerora_scene::{
    BlobShadow, Camera, DashedPath, Draw, Frame, Gpu, IMAGE_FORMAT, PointLight, Renderer, RendererConfig, Ring, render_to_image,
};

const WIDTH: u32 = 1440;
const HEIGHT: u32 = 900;

fn hex(s: &str) -> [f32; 3] {
    srgb_hex(s).expect("valid colour")
}

/// Colours of the six faces of a box.
#[derive(Clone, Copy)]
struct Faces {
    top: [f32; 3],
    bottom: [f32; 3],
    px: [f32; 3],
    nx: [f32; 3],
    pz: [f32; 3],
    nz: [f32; 3],
}

impl Faces {
    fn all(c: [f32; 3]) -> Self {
        Faces { top: c, bottom: c, px: c, nx: c, pz: c, nz: c }
    }
}

/// Adds a flat quad whose front faces `normal`, whatever order the corners
/// come in, and paints it.
fn quad(mesh: &mut Mesh, mut corners: [Vec3; 4], normal: Vec3, color: [f32; 3], emissive: f32) {
    if (corners[1] - corners[0]).cross(corners[2] - corners[0]).dot(normal) < 0.0 {
        corners.reverse();
    }
    let start = mesh.vertices.len();
    mesh.push_quad(corners[0], corners[1], corners[2], corners[3]);
    for v in &mut mesh.vertices[start..] {
        v.color = color;
        v.emissive = emissive;
    }
}

fn cuboid(mesh: &mut Mesh, min: Vec3, max: Vec3, faces: Faces, emissive: f32) {
    let (a, b) = (min, max);
    let p = |x: f32, y: f32, z: f32| Vec3::new(x, y, z);
    quad(mesh, [p(a.x, b.y, a.z), p(b.x, b.y, a.z), p(b.x, b.y, b.z), p(a.x, b.y, b.z)], Vec3::Y, faces.top, emissive);
    quad(mesh, [p(a.x, a.y, a.z), p(b.x, a.y, a.z), p(b.x, a.y, b.z), p(a.x, a.y, b.z)], -Vec3::Y, faces.bottom, emissive);
    quad(mesh, [p(b.x, a.y, a.z), p(b.x, b.y, a.z), p(b.x, b.y, b.z), p(b.x, a.y, b.z)], Vec3::X, faces.px, emissive);
    quad(mesh, [p(a.x, a.y, a.z), p(a.x, b.y, a.z), p(a.x, b.y, b.z), p(a.x, a.y, b.z)], -Vec3::X, faces.nx, emissive);
    quad(mesh, [p(a.x, a.y, b.z), p(b.x, a.y, b.z), p(b.x, b.y, b.z), p(a.x, b.y, b.z)], Vec3::Z, faces.pz, emissive);
    quad(mesh, [p(a.x, a.y, a.z), p(b.x, a.y, a.z), p(b.x, b.y, a.z), p(a.x, b.y, a.z)], -Vec3::Z, faces.nz, emissive);
}

fn solid(mesh: &mut Mesh, min: Vec3, max: Vec3, color: &str) {
    cuboid(mesh, min, max, Faces::all(hex(color)), 0.0);
}

/// The room, the slab, the garden and the furniture as one static mesh.
fn space() -> Mesh {
    let mut m = Mesh::new();
    let v = Vec3::new;

    // Dark slab under everything, its rim a little lighter than the sides.
    cuboid(&mut m, v(-24.0, -56.0, -24.0), v(924.0, 0.0, 524.0), Faces { top: hex("#343A48"), ..Faces::all(hex("#2A3040")) }, 0.0);

    // Wooden floor in planks along x.
    let planks = ["#A86F42", "#9C6640", "#B07748", "#A26B44"];
    for i in 0..10 {
        let z0 = i as f32 * 50.0;
        solid(&mut m, v(0.0, 0.0, z0), v(560.0, 6.0, z0 + 50.0), planks[i % planks.len()]);
    }

    // Garden: grass in two tones, stepping stones, a hedge along the back.
    solid(&mut m, v(560.0, 0.0, 0.0), v(900.0, 4.0, 500.0), "#4C7444");
    solid(&mut m, v(700.0, 0.0, 300.0), v(900.0, 4.5, 500.0), "#53804A");
    for (i, x) in [600.0, 650.0, 700.0, 750.0].into_iter().enumerate() {
        let z = 430.0 - i as f32 * 12.0;
        solid(&mut m, v(x, 4.0, z), v(x + 34.0, 6.0, z + 24.0), "#CFC4B2");
    }
    solid(&mut m, v(560.0, 0.0, -20.0), v(900.0, 60.0, 4.0), "#3E6A3C");

    // Tree in the garden.
    solid(&mut m, v(792.0, 4.0, 142.0), v(808.0, 110.0, 158.0), "#6B4A33");
    cuboid(&mut m, v(760.0, 100.0, 110.0), v(840.0, 170.0, 190.0), Faces::all(hex("#5E9150")), 0.0);
    cuboid(&mut m, v(775.0, 165.0, 125.0), v(825.0, 205.0, 175.0), Faces::all(hex("#6BA35C")), 0.0);

    // Bench for the resting figure.
    solid(&mut m, v(700.0, 4.0, 220.0), v(790.0, 30.0, 250.0), "#7A5A3E");

    // Two walls: warm plaster inside, dark outside, light cut edges.
    let edge = hex("#F4ECDF");
    let outside = hex("#232833");
    let plaster = hex("#EDE1CC");
    cuboid(
        &mut m,
        v(-12.0, 0.0, -12.0),
        v(0.0, 300.0, 500.0),
        Faces { top: edge, bottom: outside, px: plaster, nx: outside, pz: edge, nz: outside },
        0.0,
    );
    cuboid(
        &mut m,
        v(0.0, 0.0, -12.0),
        v(560.0, 300.0, 0.0),
        Faces { top: edge, bottom: outside, px: edge, nx: outside, pz: plaster, nz: outside },
        0.0,
    );

    // Skirting boards.
    solid(&mut m, v(0.0, 6.0, 0.0), v(3.0, 16.0, 500.0), "#C9B79C");
    solid(&mut m, v(0.0, 6.0, 0.0), v(560.0, 16.0, 3.0), "#C9B79C");

    // Left wall: door and a night window.
    solid(&mut m, v(0.0, 6.0, 300.0), v(4.0, 216.0, 390.0), "#1F6F6B");
    solid(&mut m, v(0.0, 6.0, 292.0), v(5.0, 222.0, 300.0), "#E2D5BE");
    cuboid(&mut m, v(0.0, 120.0, 110.0), v(3.0, 230.0, 230.0), Faces::all(hex("#1A2340")), 0.15);
    solid(&mut m, v(0.0, 112.0, 102.0), v(10.0, 120.0, 238.0), "#E2D5BE");

    // Right wall: bookshelf, clock, a screen.
    solid(&mut m, v(40.0, 6.0, 0.0), v(140.0, 230.0, 36.0), "#5A3B28");
    for (i, c) in ["#2E4E7A", "#8A3B32", "#C7A15A", "#3F6B4B"].into_iter().enumerate() {
        let y = 40.0 + i as f32 * 48.0;
        solid(&mut m, v(48.0, y, 4.0), v(132.0, y + 34.0, 30.0), c);
    }
    solid(&mut m, v(200.0, 230.0, 0.0), v(236.0, 266.0, 3.0), "#F7F3EA");
    cuboid(&mut m, v(380.0, 130.0, 0.0), v(470.0, 190.0, 5.0), Faces::all(hex("#11141B")), 0.0);
    cuboid(&mut m, v(388.0, 150.0, 5.0), v(462.0, 156.0, 6.0), Faces::all(hex("#F2B45A")), 1.0);

    // Workbench and router cabinet.
    solid(&mut m, v(280.0, 6.0, 30.0), v(420.0, 80.0, 90.0), "#6B4630");
    solid(&mut m, v(500.0, 6.0, 20.0), v(550.0, 150.0, 70.0), "#2B2F38");
    cuboid(&mut m, v(550.0, 120.0, 30.0), v(551.0, 126.0, 60.0), Faces::all(hex("#7FE3D0")), 1.0);

    // Desk with the glowing lamp.
    solid(&mut m, v(200.0, 6.0, 230.0), v(320.0, 76.0, 300.0), "#5C3D2A");
    solid(&mut m, v(255.0, 76.0, 255.0), v(259.0, 120.0, 259.0), "#2A2D35");
    cuboid(&mut m, v(240.0, 112.0, 240.0), v(274.0, 132.0, 274.0), Faces::all(hex("#FFD58A")), 1.0);

    // Rug.
    solid(&mut m, v(330.0, 6.0, 330.0), v(470.0, 7.0, 460.0), "#3A3F5A");
    m
}

/// A figure stand-in: body, head and two glowing eyes facing +x +z.
fn figure(body: &str, height: f32) -> Mesh {
    let mut m = Mesh::new();
    let v = Vec3::new;
    let c = hex(body);
    let shade = c.map(|x| x * 0.8);
    cuboid(&mut m, v(-16.0, 0.0, -16.0), v(16.0, height * 0.62, 16.0), Faces { top: c, ..Faces::all(shade) }, 0.0);
    cuboid(&mut m, v(-13.0, height * 0.62, -13.0), v(13.0, height, 13.0), Faces::all(c), 0.0);
    let eye_y = height * 0.82;
    cuboid(&mut m, v(5.0, eye_y, 13.0), v(9.0, eye_y + 5.0, 14.0), Faces::all(hex("#1B1E26")), 0.0);
    cuboid(&mut m, v(13.0, eye_y, 5.0), v(14.0, eye_y + 5.0, 9.0), Faces::all(hex("#1B1E26")), 0.0);
    m
}

struct Agent {
    position: Vec3,
    body: &'static str,
    height: f32,
    brightness: f32,
    rings: Vec<(f32, f32, &'static str, f32, f32, f32)>,
}

fn main() -> anyhow::Result<()> {
    let out = std::env::args_os().nth(1).map(PathBuf::from).unwrap_or_else(|| PathBuf::from("target/snapshots/scene-test.png"));
    if let Some(dir) = out.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }

    let (gpu, info) = Gpu::headless_with_info()?;
    eprintln!("adapter: {} ({:?})", info.name, info.backend);
    let mut renderer = Renderer::new(&gpu.device, RendererConfig { color_format: IMAGE_FORMAT, sample_count: 4 });

    let space_id = renderer.upload_mesh(&gpu.device, &space());
    let mut draws = vec![Draw { mesh: space_id, transform: Mat4::IDENTITY, brightness: 1.0, emissive_scale: 1.0 }];

    // Ring tuples: (height above head, radius, colour, intensity, arc start, arc length).
    let working = |start| (14.0, 22.0, "#F2B45A", 1.0, start, PI);
    let agents = [
        Agent {
            position: Vec3::new(400.0, 7.0, 420.0),
            body: "#EFE6D8",
            height: 78.0,
            brightness: 1.0,
            rings: vec![(14.0, 24.0, "#8CD3FF", 1.0, 0.0, TAU)],
        },
        Agent { position: Vec3::new(350.0, 6.0, 130.0), body: "#C8743A", height: 74.0, brightness: 1.0, rings: vec![working(0.6)] },
        Agent { position: Vec3::new(110.0, 6.0, 90.0), body: "#CFC7F2", height: 70.0, brightness: 1.0, rings: vec![working(2.2)] },
        Agent {
            position: Vec3::new(470.0, 6.0, 150.0),
            body: "#D9DCE3",
            height: 80.0,
            brightness: 1.0,
            rings: vec![(12.0, 22.0, "#9B8CF0", 1.0, 0.0, TAU), (20.0, 17.0, "#9B8CF0", 1.0, 0.0, TAU)],
        },
        Agent {
            position: Vec3::new(745.0, 30.0, 235.0),
            body: "#3A4050",
            height: 72.0,
            brightness: 0.6,
            rings: vec![(14.0, 22.0, "#5B6170", 0.35, 0.0, TAU)],
        },
    ];

    let mut rings = Vec::new();
    let mut shadows = Vec::new();
    for agent in &agents {
        let id = renderer.upload_mesh(&gpu.device, &figure(agent.body, agent.height));
        draws.push(Draw { mesh: id, transform: Mat4::from_translation(agent.position), brightness: agent.brightness, emissive_scale: 1.0 });
        shadows.push(BlobShadow { center: agent.position, radius: 40.0, opacity: 0.55 });
        for &(above, radius, color, intensity, arc_start, arc_len) in &agent.rings {
            rings.push(Ring {
                center: agent.position + Vec3::Y * (agent.height + above),
                radius,
                width: 3.0,
                color: hex(color),
                intensity,
                arc_start,
                arc_len,
            });
        }
    }

    let frame = Frame {
        camera: Camera {
            target: Vec3::new(450.0, 110.0, 250.0),
            azimuth_deg: 45.0,
            elevation_deg: 30.0,
            view_height: 1000.0,
            aspect: WIDTH as f32 / HEIGHT as f32,
        },
        clear_color: hex("#0B0D12"),
        ambient_sky: [0.30, 0.32, 0.40],
        ambient_ground: [0.12, 0.10, 0.10],
        sun_direction: Vec3::new(-0.35, -0.75, -0.55),
        sun_color: [1.0, 0.9, 0.8],
        sun_intensity: 0.55,
        lights: vec![PointLight { position: Vec3::new(257.0, 150.0, 257.0), color: [1.0, 0.76, 0.48], intensity: 0.55, radius: 650.0 }],
        draws,
        rings,
        shadows,
        paths: vec![DashedPath {
            points: vec![
                Vec3::new(260.0, 6.0, 380.0),
                Vec3::new(330.0, 6.0, 300.0),
                Vec3::new(430.0, 6.0, 250.0),
                Vec3::new(470.0, 6.0, 175.0),
            ],
            width: 6.0,
            dash: 12.0,
            gap: 8.0,
            color: hex("#9B8CF0"),
            opacity: 0.9,
        }],
    };

    let start = std::time::Instant::now();
    let image = render_to_image(&gpu, &mut renderer, &frame, WIDTH, HEIGHT)?;
    eprintln!("rendered {WIDTH}x{HEIGHT} in {:.1} ms", start.elapsed().as_secs_f64() * 1000.0);
    image.save(&out).with_context(|| format!("writing {}", out.display()))?;
    println!("{}", out.display());
    Ok(())
}
