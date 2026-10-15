use std::collections::HashMap;

use meerora_geom::prim::*;
use meerora_geom::{Mesh, Vec2, Vec3};

fn triangles(m: &Mesh) -> Vec<[Vec3; 3]> {
    m.indices.as_chunks::<3>().0.iter().map(|t| t.map(|i| Vec3::from(m.vertices[i as usize].position))).collect()
}

fn key(p: Vec3) -> [i64; 3] {
    (p * 1000.0).round().to_array().map(|v| v as i64)
}

/// Every directed edge appears once and its reverse appears once. That means
/// every edge is shared by exactly two triangles and the winding is consistent.
fn assert_closed(name: &str, m: &Mesh) {
    let mut edges: HashMap<([i64; 3], [i64; 3]), usize> = HashMap::new();
    for t in triangles(m) {
        for k in 0..3 {
            *edges.entry((key(t[k]), key(t[(k + 1) % 3]))).or_default() += 1;
        }
    }
    for (&(a, b), &count) in &edges {
        assert_eq!(count, 1, "{name}: edge {a:?} -> {b:?} used {count} times");
        assert_eq!(edges.get(&(b, a)), Some(&1), "{name}: edge {a:?} -> {b:?} has no matching reverse edge");
    }
}

/// Normals have unit length and agree with the counter-clockwise winding.
fn assert_normals(name: &str, m: &Mesh) {
    for (i, t) in m.indices.as_chunks::<3>().0.iter().enumerate() {
        let p = [0, 1, 2].map(|k| Vec3::from(m.vertices[t[k] as usize].position));
        let face = (p[1] - p[0]).cross(p[2] - p[0]).normalize();
        for &v in t {
            let n = Vec3::from(m.vertices[v as usize].normal);
            assert!((n.length() - 1.0).abs() < 1e-4, "{name}: normal {n} of triangle {i} is not unit length");
            assert!((n - face).length() < 1e-3, "{name}: normal {n} of triangle {i} disagrees with winding {face}");
        }
    }
}

/// For convex solids every face points away from the centre of the bounds.
fn assert_outward(name: &str, m: &Mesh) {
    let centre = m.bounds().center();
    for (i, t) in triangles(m).into_iter().enumerate() {
        let n = (t[1] - t[0]).cross(t[2] - t[0]);
        let mid = (t[0] + t[1] + t[2]) / 3.0;
        assert!(n.dot(mid - centre) > 0.0, "{name}: triangle {i} faces inwards");
    }
}

fn assert_grounded(name: &str, m: &Mesh) {
    let b = m.bounds();
    assert!(b.min.y.abs() < 1e-4, "{name}: lowest point at y = {}", b.min.y);
    assert!((b.min.x + b.max.x).abs() < 1e-3, "{name}: not centred in x ({} .. {})", b.min.x, b.max.x);
    assert!((b.min.z + b.max.z).abs() < 1e-3, "{name}: not centred in z ({} .. {})", b.min.z, b.max.z);
}

fn assert_white(name: &str, m: &Mesh) {
    assert!(m.vertices.iter().all(|v| v.color == [1.0, 1.0, 1.0] && v.emissive == 0.0), "{name}: not plain white");
}

fn convex_solids() -> Vec<(&'static str, Mesh)> {
    vec![
        ("cuboid", cuboid(4.0, 6.0, 2.0)),
        ("bevel_box", bevel_box(48.0, 56.0, 40.0, 6.0)),
        ("cylinder", cylinder(3.0, 5.0, 12)),
        ("frustum", frustum(4.0, 2.0, 5.0, 10)),
        ("cone", frustum(4.0, 0.0, 5.0, 8)),
        ("sphere0", sphere(2.0, 0)),
        ("sphere2", sphere(2.0, 2)),
        ("capsule", capsule(3.0, 10.0, 10)),
        ("capsule_round", capsule(3.0, 6.0, 8)),
        ("egg", egg(52.0, 80.0, 12)),
        ("lathe", lathe(&[Vec2::new(3.0, 1.0), Vec2::new(4.0, 3.0), Vec2::new(2.0, 6.0), Vec2::new(0.0, 7.0)], 10)),
    ]
}

#[test]
fn triangle_counts() {
    assert_eq!(cuboid(1.0, 2.0, 3.0).triangle_count(), 12);
    assert_eq!(bevel_box(4.0, 4.0, 4.0, 0.5).triangle_count(), 44);
    assert_eq!(bevel_box(4.0, 4.0, 4.0, 0.0).triangle_count(), 12);
    // 2 per side quad plus a fan of n - 2 on each cap.
    assert_eq!(cylinder(1.0, 1.0, 12).triangle_count(), 12 * 2 + 2 * 10);
    assert_eq!(frustum(2.0, 1.0, 1.0, 8).triangle_count(), 8 * 2 + 2 * 6);
    assert_eq!(frustum(2.0, 0.0, 1.0, 8).triangle_count(), 8 + 6);
    assert_eq!(sphere(1.0, 0).triangle_count(), 20);
    assert_eq!(sphere(1.0, 1).triangle_count(), 80);
    assert_eq!(sphere(1.0, 3).triangle_count(), 1280);
    assert_eq!(torus(5.0, 1.0, 16, 6).triangle_count(), 192);
    assert_eq!(quad(2.0, 3.0).triangle_count(), 2);
    // A capsule with 8 segments has 2 rings per cap. That gives 2 tip fans and
    // 3 full bands, one in each cap and one for the cylinder.
    assert_eq!(capsule(1.0, 4.0, 8).triangle_count(), 2 * 8 + 3 * 16);
    assert_eq!(egg(2.0, 3.0, 12).triangle_count(), 2 * 12 + 4 * 24);
    // A capsule of exactly 2 * radius has no cylinder band.
    assert_eq!(capsule(1.0, 2.0, 8).triangle_count(), 2 * 8 + 2 * 16);
}

#[test]
fn normals_are_unit_and_match_winding() {
    let mut all = convex_solids();
    all.push(("torus", torus(5.0, 1.0, 12, 5)));
    all.push(("quad", quad(2.0, 3.0)));
    all.push(("odd frustum", frustum(4.0, 2.0, 5.0, 9)));
    all.push(("odd cone", frustum(4.0, 0.0, 5.0, 7)));
    all.push(("tube", tube(&[Vec3::ZERO, Vec3::new(0.0, 10.0, 0.0), Vec3::new(6.0, 14.0, 2.0)], 1.0, 6)));
    all.push(("extrude", extrude(&l_shape(), 2.0)));
    for (name, m) in &all {
        assert!(!m.is_empty(), "{name}: empty");
        assert_normals(name, m);
        assert_white(name, m);
    }
}

#[test]
fn convex_solids_face_outwards() {
    for (name, m) in &convex_solids() {
        assert_outward(name, m);
    }
}

#[test]
fn solids_are_closed() {
    let mut all = convex_solids();
    all.push(("torus", torus(5.0, 1.0, 12, 5)));
    for (name, m) in &all {
        assert_closed(name, m);
    }
}

#[test]
fn solids_sit_on_the_ground_centred() {
    for (name, m) in &convex_solids() {
        assert_grounded(name, m);
    }
    let b = sphere(2.0, 3).bounds();
    assert!((b.max.y - 4.0).abs() < 1e-4);
    let b = cuboid(4.0, 6.0, 2.0).bounds();
    assert_eq!((b.min, b.max), (Vec3::new(-2.0, 0.0, -1.0), Vec3::new(2.0, 6.0, 1.0)));
    let b = egg(52.0, 80.0, 16).bounds();
    assert!((b.max.y - 80.0).abs() < 1e-3);
    assert!(b.size().x <= 52.0 + 1e-3 && b.size().x > 48.0, "egg width {}", b.size().x);
}

#[test]
fn egg_is_wider_below_the_middle() {
    let m = egg(50.0, 80.0, 16);
    let widest = m.vertices.iter().max_by(|a, b| a.position[0].total_cmp(&b.position[0])).unwrap();
    assert!(widest.position[1] < 40.0, "widest at y = {}", widest.position[1]);
}

#[test]
fn torus_lies_flat_around_the_origin() {
    let m = torus(10.0, 2.0, 16, 8);
    let b = m.bounds();
    assert!((b.min.y + 2.0).abs() < 1e-4 && (b.max.y - 2.0).abs() < 1e-4);
    assert!((b.max.z - 12.0).abs() < 1e-3 && (b.min.x + b.max.x).abs() < 1e-3);
    // Faces point away from the core circle.
    for t in triangles(&m) {
        let mid = (t[0] + t[1] + t[2]) / 3.0;
        let core = Vec3::new(mid.x, 0.0, mid.z).normalize() * 10.0;
        assert!((t[1] - t[0]).cross(t[2] - t[0]).dot(mid - core) > 0.0);
    }
}

#[test]
fn quad_faces_up() {
    let m = quad(4.0, 2.0);
    assert!(m.vertices.iter().all(|v| v.normal == [0.0, 1.0, 0.0] && v.position[1] == 0.0));
    let b = m.bounds();
    assert_eq!((b.min, b.max), (Vec3::new(-2.0, 0.0, -1.0), Vec3::new(2.0, 0.0, 1.0)));
}

fn l_shape() -> Vec<Vec2> {
    vec![Vec2::new(0.0, 0.0), Vec2::new(6.0, 0.0), Vec2::new(6.0, 2.0), Vec2::new(2.0, 2.0), Vec2::new(2.0, 5.0), Vec2::new(0.0, 5.0)]
}

#[test]
fn concave_extrude() {
    let m = extrude(&l_shape(), 2.0);
    // 6 side quads plus 4 triangles on each cap.
    assert_eq!(m.triangle_count(), 6 * 2 + 2 * 4);
    assert_closed("extrude", &m);
    let b = m.bounds();
    assert_eq!((b.min, b.max), (Vec3::new(0.0, 0.0, -1.0), Vec3::new(6.0, 5.0, 1.0)));
    // The caps cover exactly the L (area 6 * 2 + 2 * 3 = 18), so no triangle
    // spans the notch.
    let mut front = 0.0;
    let mut back = 0.0;
    for t in triangles(&m) {
        let n = (t[1] - t[0]).cross(t[2] - t[0]);
        let mid = (t[0] + t[1] + t[2]) / 3.0;
        if n.z > 0.0 && n.x.abs() < 1e-6 && n.y.abs() < 1e-6 {
            assert!((mid.z - 1.0).abs() < 1e-6);
            front += 0.5 * n.length();
        } else if n.z < 0.0 && n.x.abs() < 1e-6 && n.y.abs() < 1e-6 {
            assert!((mid.z + 1.0).abs() < 1e-6);
            back += 0.5 * n.length();
        } else {
            // Side faces are vertical and point away from the inside of the L.
            assert!(n.z.abs() < 1e-6);
            let probe = Vec2::new(mid.x, mid.y) - Vec2::new(n.x, n.y).normalize() * 0.01;
            assert!(in_l(probe), "side face at {mid} points inwards");
        }
        assert!(!(mid.x > 2.0 + 1e-6 && mid.y > 2.0 + 1e-6), "triangle in the notch at {mid}");
    }
    assert!((front - 18.0).abs() < 1e-4 && (back - 18.0).abs() < 1e-4, "cap areas {front} and {back}");
}

fn in_l(p: Vec2) -> bool {
    (0.0..=6.0).contains(&p.x) && (0.0..=2.0).contains(&p.y) || (0.0..=2.0).contains(&p.x) && (0.0..=5.0).contains(&p.y)
}

#[test]
fn clockwise_outline_is_turned_around() {
    let mut cw = l_shape();
    cw.reverse();
    let m = extrude(&cw, 2.0);
    assert_eq!(m.triangle_count(), 20);
    assert_closed("extrude cw", &m);
}

#[test]
fn three_point_tube() {
    let path = [Vec3::new(1.0, 0.0, 0.0), Vec3::new(1.0, 10.0, 0.0), Vec3::new(8.0, 14.0, 3.0)];
    let m = tube(&path, 1.5, 6);
    // 2 segments of 6 quads plus a fan of 4 on each cap.
    assert_eq!(m.triangle_count(), 2 * 6 * 2 + 2 * 4);
    assert_closed("tube", &m);
    assert_normals("tube", &m);
    // The path is used as given, so the first cap is centred on the first point.
    let first: Vec<Vec3> = m.vertices.iter().map(|v| Vec3::from(v.position)).filter(|p| p.y.abs() < 1e-5).collect();
    assert!(!first.is_empty());
    for p in &first {
        assert!(((*p - path[0]).length() - 1.5).abs() < 1e-4);
    }
    // Side faces point away from the path, the caps point along it.
    let (start, end) = ((path[0] - path[1]).normalize(), (path[2] - path[1]).normalize());
    for t in triangles(&m) {
        let mid = (t[0] + t[1] + t[2]) / 3.0;
        let n = (t[1] - t[0]).cross(t[2] - t[0]).normalize();
        if (mid - path[0]).dot(start).abs() < 1e-4 && n.dot(start) > 0.99 || (mid - path[2]).dot(end).abs() < 1e-4 && n.dot(end) > 0.99 {
            continue;
        }
        let off = (mid - nearest_on_path(&path, mid)).normalize();
        assert!(n.dot(off) > 0.5, "tube face at {mid} points inwards");
    }
}

fn nearest_on_path(path: &[Vec3], p: Vec3) -> Vec3 {
    path.windows(2)
        .map(|w| {
            let d = w[1] - w[0];
            w[0] + d * ((p - w[0]).dot(d) / d.length_squared()).clamp(0.0, 1.0)
        })
        .min_by(|a, b| a.distance(p).total_cmp(&b.distance(p)))
        .unwrap()
}

#[test]
fn tube_mitre_stays_inside_the_bend() {
    // At a right-angle bend the mitred joint ring lies on the 45 degree plane.
    let path = [Vec3::ZERO, Vec3::new(0.0, 10.0, 0.0), Vec3::new(10.0, 10.0, 0.0)];
    let m = tube(&path, 1.0, 8);
    assert_closed("bent tube", &m);
    let b = m.bounds();
    assert!((b.max.y - 11.0).abs() < 1e-3 && (b.min.x + 1.0).abs() < 1e-3, "{b:?}");
}

#[test]
fn degenerate_inputs_give_empty_meshes() {
    assert!(tube(&[Vec3::ZERO], 1.0, 6).is_empty());
    assert!(tube(&[Vec3::ZERO, Vec3::ZERO], 1.0, 6).is_empty());
    assert!(lathe(&[Vec2::new(1.0, 0.0)], 8).is_empty());
    assert!(extrude(&[Vec2::ZERO, Vec2::X], 1.0).is_empty());
}

#[test]
fn small_segment_counts_are_clamped() {
    assert_eq!(cylinder(1.0, 1.0, 0).triangle_count(), cylinder(1.0, 1.0, 3).triangle_count());
    assert_eq!(frustum(1.0, 0.5, 1.0, 2).triangle_count(), frustum(1.0, 0.5, 1.0, 3).triangle_count());
    assert_eq!(lathe(&[Vec2::new(1.0, 0.0), Vec2::new(1.0, 1.0)], 1).triangle_count(), cylinder(1.0, 1.0, 3).triangle_count());
    assert_eq!(sphere(1.0, 9).triangle_count(), 20 * 4usize.pow(4));
    assert_eq!(capsule(1.0, 3.0, 1).triangle_count(), capsule(1.0, 3.0, 4).triangle_count());
    assert_eq!(egg(1.0, 2.0, 0).triangle_count(), egg(1.0, 2.0, 4).triangle_count());
    assert_eq!(torus(3.0, 1.0, 1, 2).triangle_count(), torus(3.0, 1.0, 3, 3).triangle_count());
    assert_eq!(tube(&[Vec3::ZERO, Vec3::Y], 1.0, 2).triangle_count(), tube(&[Vec3::ZERO, Vec3::Y], 1.0, 3).triangle_count());
    // A capsule shorter than its diameter becomes a sphere-like capsule.
    assert_eq!(capsule(1.0, 0.5, 8).bounds().max.y, 2.0);
    for (name, m) in [("cyl", cylinder(1.0, 1.0, 0)), ("capsule", capsule(1.0, 3.0, 1)), ("torus", torus(3.0, 1.0, 1, 1))] {
        assert_closed(name, &m);
    }
}

#[test]
fn bevel_is_clamped() {
    let m = bevel_box(4.0, 2.0, 4.0, 10.0);
    let b = m.bounds();
    assert_eq!((b.min, b.max), (Vec3::new(-2.0, 0.0, -2.0), Vec3::new(2.0, 2.0, 2.0)));
    assert_normals("bevel clamped", &m);
}
