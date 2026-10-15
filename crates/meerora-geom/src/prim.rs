//! Primitive shapes. All of them are flat shaded, painted white and closed
//! unless noted. Solids sit on the ground plane (lowest point at y = 0) and are
//! centred on the y axis, except [`torus`], [`tube`] and [`extrude`], whose
//! placement is documented on the function.
//!
//! `segments` and `detail` arguments control the triangle count. Values below
//! the documented minimum are clamped to it.

use std::f32::consts::{FRAC_PI_2, PI, TAU};

use crate::{Mesh, Quat, Vec2, Vec3};

/// Box of width (x), height (y) and depth (z). 12 triangles.
pub fn cuboid(w: f32, h: f32, d: f32) -> Mesh {
    let (x, z) = (w * 0.5, d * 0.5);
    let centre = Vec3::new(0.0, h * 0.5, 0.0);
    let p = |sx: f32, sy: f32, sz: f32| Vec3::new(sx * x, if sy > 0.0 { h } else { 0.0 }, sz * z);
    let mut m = Mesh::new();
    for s in [-1.0, 1.0] {
        quad_out(&mut m, [p(s, -1.0, -1.0), p(s, 1.0, -1.0), p(s, 1.0, 1.0), p(s, -1.0, 1.0)], centre);
        quad_out(&mut m, [p(-1.0, s, -1.0), p(1.0, s, -1.0), p(1.0, s, 1.0), p(-1.0, s, 1.0)], centre);
        quad_out(&mut m, [p(-1.0, -1.0, s), p(1.0, -1.0, s), p(1.0, 1.0, s), p(-1.0, 1.0, s)], centre);
    }
    m
}

/// Box whose edges and corners are chamfered by `bevel` (clamped to half the
/// smallest side). 44 triangles when `bevel > 0`, otherwise a plain cuboid.
pub fn bevel_box(w: f32, h: f32, d: f32, bevel: f32) -> Mesh {
    let b = bevel.min(w.min(h).min(d) * 0.5);
    if b.is_nan() || b <= 0.0 {
        return cuboid(w, h, d);
    }
    let (hx, hy, hz) = (w * 0.5, h * 0.5, d * 0.5);
    let centre = Vec3::new(0.0, hy, 0.0);
    // Each corner of the box is cut into a small triangle with one vertex on
    // each of the three faces that meet there.
    let xv = |sx: f32, sy: f32, sz: f32| centre + Vec3::new(sx * hx, sy * (hy - b), sz * (hz - b));
    let yv = |sx: f32, sy: f32, sz: f32| centre + Vec3::new(sx * (hx - b), sy * hy, sz * (hz - b));
    let zv = |sx: f32, sy: f32, sz: f32| centre + Vec3::new(sx * (hx - b), sy * (hy - b), sz * hz);
    let signs = [-1.0, 1.0];
    let mut m = Mesh::new();
    for s in signs {
        quad_out(&mut m, [xv(s, -1.0, -1.0), xv(s, 1.0, -1.0), xv(s, 1.0, 1.0), xv(s, -1.0, 1.0)], centre);
        quad_out(&mut m, [yv(-1.0, s, -1.0), yv(1.0, s, -1.0), yv(1.0, s, 1.0), yv(-1.0, s, 1.0)], centre);
        quad_out(&mut m, [zv(-1.0, -1.0, s), zv(1.0, -1.0, s), zv(1.0, 1.0, s), zv(-1.0, 1.0, s)], centre);
    }
    for s in signs {
        for t in signs {
            quad_out(&mut m, [xv(s, t, -1.0), yv(s, t, -1.0), yv(s, t, 1.0), xv(s, t, 1.0)], centre);
            quad_out(&mut m, [yv(-1.0, s, t), zv(-1.0, s, t), zv(1.0, s, t), yv(1.0, s, t)], centre);
            quad_out(&mut m, [xv(s, -1.0, t), zv(s, -1.0, t), zv(s, 1.0, t), xv(s, 1.0, t)], centre);
        }
    }
    for sx in signs {
        for sy in signs {
            for sz in signs {
                tri_out(&mut m, [xv(sx, sy, sz), yv(sx, sy, sz), zv(sx, sy, sz)], centre);
            }
        }
    }
    m
}

/// Upright cylinder. Minimum 3 segments.
pub fn cylinder(radius: f32, height: f32, segments: u32) -> Mesh {
    frustum(radius, radius, height, segments)
}

/// Truncated cone. `r_top = 0` makes a cone. Minimum 3 segments.
pub fn frustum(r_bottom: f32, r_top: f32, height: f32, segments: u32) -> Mesh {
    lathe(&[Vec2::new(r_bottom, 0.0), Vec2::new(r_top, height)], segments)
}

/// Surface of revolution around the y axis. `profile` points are
/// (radius, height) from bottom to top. Ends with a non-zero radius get a flat
/// cap. Minimum 3 segments and 2 profile points.
pub fn lathe(profile: &[Vec2], segments: u32) -> Mesh {
    let mut m = Mesh::new();
    if profile.len() < 2 {
        return m;
    }
    let n = segments.max(3) as usize;
    let min_y = profile.iter().map(|p| p.y).fold(f32::INFINITY, f32::min);
    let angles: Vec<(f32, f32)> = (0..n).map(|i| (TAU * i as f32 / n as f32).sin_cos()).collect();
    let ring = |p: Vec2, i: usize| {
        let (s, c) = angles[i % n];
        let r = p.x.max(0.0);
        Vec3::new(r * s, p.y - min_y, r * c)
    };
    for w in profile.windows(2) {
        for i in 0..n {
            m.push_quad(ring(w[0], i), ring(w[0], i + 1), ring(w[1], i + 1), ring(w[1], i));
        }
    }
    let (first, last) = (profile[0], profile[profile.len() - 1]);
    if first.x > 0.0 {
        for k in 1..n - 1 {
            m.push_triangle(ring(first, 0), ring(first, k + 1), ring(first, k));
        }
    }
    if last.x > 0.0 {
        for k in 1..n - 1 {
            m.push_triangle(ring(last, 0), ring(last, k), ring(last, k + 1));
        }
    }
    m
}

/// Icosphere resting on y = 0. Detail 0 has 20 triangles, each level
/// multiplies by 4. Maximum detail 4.
pub fn sphere(radius: f32, detail: u32) -> Mesh {
    // Icosahedron with a vertex at each pole, so the lowest point is exactly
    // at y = -1 before the sphere is lifted onto the ground.
    let (ry, rr) = (1.0 / 5f32.sqrt(), 2.0 / 5f32.sqrt());
    let mut v = vec![Vec3::Y];
    for k in 0..5 {
        let (s, c) = (TAU * k as f32 / 5.0).sin_cos();
        v.push(Vec3::new(rr * s, ry, rr * c));
    }
    for k in 0..5 {
        let (s, c) = (TAU * (k as f32 + 0.5) / 5.0).sin_cos();
        v.push(Vec3::new(rr * s, -ry, rr * c));
    }
    v.push(Vec3::NEG_Y);
    let mut tris = Vec::with_capacity(20 << (2 * detail.min(4)));
    for k in 0..5 {
        let (u0, u1, l0, l1) = (1 + k, 1 + (k + 1) % 5, 6 + k, 6 + (k + 1) % 5);
        tris.push([v[0], v[u0], v[u1]]);
        tris.push([v[u0], v[l0], v[u1]]);
        tris.push([v[u1], v[l0], v[l1]]);
        tris.push([v[l0], v[11], v[l1]]);
    }
    for _ in 0..detail.min(4) {
        let mut next = Vec::with_capacity(tris.len() * 4);
        for [a, b, c] in tris {
            let (ab, bc, ca) = ((a + b).normalize(), (b + c).normalize(), (c + a).normalize());
            next.extend_from_slice(&[[a, ab, ca], [ab, b, bc], [ca, bc, c], [ab, bc, ca]]);
        }
        tris = next;
    }
    let lift = Vec3::new(0.0, radius, 0.0);
    let mut m = Mesh::new();
    for t in tris {
        tri_out(&mut m, t.map(|p| p * radius + lift), lift);
    }
    m
}

/// Capsule of total height `height` (at least `2 * radius`). Minimum 4 segments.
pub fn capsule(radius: f32, height: f32, segments: u32) -> Mesh {
    let n = segments.max(4);
    let r = radius.max(0.0);
    let h = height.max(2.0 * r);
    let steps = (n / 4).max(1);
    let mut profile = Vec::with_capacity(2 * steps as usize + 2);
    for i in 0..=steps {
        let a = -FRAC_PI_2 + FRAC_PI_2 * i as f32 / steps as f32;
        let x = if i == 0 { 0.0 } else { r * a.cos() };
        profile.push(Vec2::new(x, r + r * a.sin()));
    }
    for i in 0..=steps {
        let a = FRAC_PI_2 * i as f32 / steps as f32;
        let x = if i == steps { 0.0 } else { r * a.cos() };
        profile.push(Vec2::new(x, h - r + r * a.sin()));
    }
    lathe(&profile, n)
}

/// Egg of the given width and height, wider below the middle. Minimum 4 segments.
pub fn egg(width: f32, height: f32, segments: u32) -> Mesh {
    const K: f32 = 0.25;
    let n = segments.max(4);
    let steps = (n / 2).max(3);
    // r(a) = sin(a) * (1 + K cos(a)) peaks below the middle. Scale it so the
    // widest ring has the requested width.
    let c = ((1.0 + 8.0 * K * K).sqrt() - 1.0) / (4.0 * K);
    let peak = (1.0 - c * c).sqrt() * (1.0 + K * c);
    let profile: Vec<Vec2> = (0..=steps)
        .map(|i| {
            let a = PI * i as f32 / steps as f32;
            let (s, co) = a.sin_cos();
            let r = if i == 0 || i == steps { 0.0 } else { 0.5 * width * s * (1.0 + K * co) / peak };
            Vec2::new(r, 0.5 * height * (1.0 - co))
        })
        .collect();
    lathe(&profile, n)
}

/// Ring lying in the xz plane, centred on the origin. Minimum 3 segments each.
pub fn torus(major: f32, minor: f32, major_segments: u32, minor_segments: u32) -> Mesh {
    let (nu, nv) = (major_segments.max(3), minor_segments.max(3));
    let core = |i: u32| {
        let (su, cu) = (TAU * (i % nu) as f32 / nu as f32).sin_cos();
        Vec3::new(su, 0.0, cu)
    };
    let p = |i: u32, j: u32| {
        let (sv, cv) = (TAU * (j % nv) as f32 / nv as f32).sin_cos();
        core(i) * (major + minor * cv) + Vec3::new(0.0, minor * sv, 0.0)
    };
    let mut m = Mesh::new();
    for i in 0..nu {
        // Midpoint of the core circle's chord, which stays inside the tube
        // even for very few major segments.
        let inside = (core(i) + core(i + 1)) * 0.5 * major;
        for j in 0..nv {
            quad_out(&mut m, [p(i, j), p(i + 1, j), p(i + 1, j + 1), p(i, j + 1)], inside);
        }
    }
    m
}

/// Tube swept along a polyline with mitred joints and capped ends. The path is
/// used as given (no recentring). Minimum 3 sides and 2 points.
pub fn tube(path: &[Vec3], radius: f32, sides: u32) -> Mesh {
    let mut pts: Vec<Vec3> = Vec::with_capacity(path.len());
    for &p in path {
        if pts.last().is_none_or(|q: &Vec3| q.distance_squared(p) > 1e-12) {
            pts.push(p);
        }
    }
    let mut m = Mesh::new();
    if pts.len() < 2 {
        return m;
    }
    let n = sides.max(3) as usize;
    let dirs: Vec<Vec3> = pts.windows(2).map(|w| (w[1] - w[0]).normalize()).collect();
    // Frame (u, v, t) with u x v = t, carried along the path by parallel transport.
    let t0 = dirs[0];
    let reference = if t0.y.abs() < 0.9 { Vec3::Y } else { Vec3::X };
    let mut u = reference.cross(t0).normalize();
    let circle: Vec<(f32, f32)> = (0..n).map(|k| (TAU * k as f32 / n as f32).sin_cos()).collect();
    let mut rings: Vec<Vec<Vec3>> = Vec::with_capacity(pts.len());
    for (i, &p) in pts.iter().enumerate() {
        let t_in = if i == 0 { dirs[0] } else { dirs[i - 1] };
        let v = t_in.cross(u);
        let mut ring: Vec<Vec3> = circle.iter().map(|&(s, c)| p + radius * (c * u + s * v)).collect();
        if i > 0 && i < dirs.len() {
            let t_out = dirs[i];
            let mitre = (t_in + t_out).normalize_or_zero();
            let along = t_in.dot(mitre);
            if mitre != Vec3::ZERO && along > 1e-4 {
                // Slide each point along the incoming direction onto the mitre
                // plane. The stretch is limited so hairpin bends stay bounded.
                let along = along.max(0.25);
                for q in &mut ring {
                    *q -= t_in * ((*q - p).dot(mitre) / along);
                }
            }
            u = (Quat::from_rotation_arc(t_in, t_out) * u).normalize();
        }
        rings.push(ring);
    }
    for w in rings.windows(2) {
        for k in 0..n {
            let k1 = (k + 1) % n;
            m.push_quad(w[0][k], w[0][k1], w[1][k1], w[1][k]);
        }
    }
    let (first, last) = (&rings[0], &rings[rings.len() - 1]);
    for k in 1..n - 1 {
        m.push_triangle(first[0], first[k + 1], first[k]);
        m.push_triangle(last[0], last[k], last[k + 1]);
    }
    m
}

/// Extrudes a simple polygon (counter-clockwise, may be concave) drawn in the
/// xy plane along z, from `-depth / 2` to `depth / 2`. The outline keeps its
/// coordinates.
pub fn extrude(outline: &[Vec2], depth: f32) -> Mesh {
    let mut pts: Vec<Vec2> = Vec::with_capacity(outline.len());
    for &p in outline {
        if pts.last().is_none_or(|q: &Vec2| q.distance_squared(p) > 1e-12) {
            pts.push(p);
        }
    }
    while pts.len() > 1 && pts[0].distance_squared(pts[pts.len() - 1]) <= 1e-12 {
        pts.pop();
    }
    let mut m = Mesh::new();
    if pts.len() < 3 {
        return m;
    }
    if signed_area(&pts) < 0.0 {
        pts.reverse();
    }
    let hz = depth * 0.5;
    for [a, b, c] in ear_clip(&pts) {
        let (a, b, c) = (pts[a], pts[b], pts[c]);
        m.push_triangle(a.extend(hz), b.extend(hz), c.extend(hz));
        m.push_triangle(a.extend(-hz), c.extend(-hz), b.extend(-hz));
    }
    for i in 0..pts.len() {
        let (p, q) = (pts[i], pts[(i + 1) % pts.len()]);
        m.push_quad(p.extend(-hz), q.extend(-hz), q.extend(hz), p.extend(hz));
    }
    m
}

/// Horizontal rectangle at y = 0 facing up, centred on the origin. One sided,
/// 2 triangles.
pub fn quad(w: f32, d: f32) -> Mesh {
    let (x, z) = (w * 0.5, d * 0.5);
    let mut m = Mesh::new();
    m.push_quad(Vec3::new(-x, 0.0, -z), Vec3::new(-x, 0.0, z), Vec3::new(x, 0.0, z), Vec3::new(x, 0.0, -z));
    m
}

/// Pushes a planar convex quad, reversed if needed so its normal points away
/// from `inside`.
fn quad_out(m: &mut Mesh, [a, b, c, d]: [Vec3; 4], inside: Vec3) {
    let n = (c - a).cross(d - b);
    if n.dot((a + b + c + d) * 0.25 - inside) >= 0.0 { m.push_quad(a, b, c, d) } else { m.push_quad(a, d, c, b) }
}

/// Pushes a triangle, reversed if needed so its normal points away from `inside`.
fn tri_out(m: &mut Mesh, [a, b, c]: [Vec3; 3], inside: Vec3) {
    let n = (b - a).cross(c - a);
    if n.dot((a + b + c) / 3.0 - inside) >= 0.0 { m.push_triangle(a, b, c) } else { m.push_triangle(a, c, b) }
}

fn signed_area(pts: &[Vec2]) -> f32 {
    let n = pts.len();
    0.5 * (0..n).map(|i| pts[i].perp_dot(pts[(i + 1) % n])).sum::<f32>()
}

/// Triangulates a counter-clockwise simple polygon by ear clipping. Returns
/// index triples, counter-clockwise.
fn ear_clip(pts: &[Vec2]) -> Vec<[usize; 3]> {
    let cross = |a: Vec2, b: Vec2, c: Vec2| (b - a).perp_dot(c - a);
    let inside = |p: Vec2, a: Vec2, b: Vec2, c: Vec2| cross(a, b, p) >= 0.0 && cross(b, c, p) >= 0.0 && cross(c, a, p) >= 0.0;
    let mut idx: Vec<usize> = (0..pts.len()).collect();
    let mut out = Vec::with_capacity(pts.len().saturating_sub(2));
    while idx.len() > 3 {
        let n = idx.len();
        let corner = |i: usize| (idx[(i + n - 1) % n], idx[i], idx[(i + 1) % n]);
        let ear = (0..n).find(|&i| {
            let (ia, ib, ic) = corner(i);
            let (a, b, c) = (pts[ia], pts[ib], pts[ic]);
            cross(a, b, c) > 0.0
                && !idx.iter().any(|&j| {
                    let p = pts[j];
                    j != ia && j != ib && j != ic && p != a && p != b && p != c && inside(p, a, b, c)
                })
        });
        match ear {
            Some(i) => {
                let (ia, ib, ic) = corner(i);
                out.push([ia, ib, ic]);
                idx.remove(i);
            }
            None => {
                // Only a degenerate or self-touching outline gets here. Drop
                // the flattest corner so the loop always ends.
                let i = (0..n)
                    .min_by(|&i, &j| {
                        let f = |k: usize| {
                            let (a, b, c) = corner(k);
                            cross(pts[a], pts[b], pts[c]).abs()
                        };
                        f(i).total_cmp(&f(j))
                    })
                    .unwrap_or(0);
                let (ia, ib, ic) = corner(i);
                if cross(pts[ia], pts[ib], pts[ic]) > 0.0 {
                    out.push([ia, ib, ic]);
                }
                idx.remove(i);
            }
        }
    }
    if idx.len() == 3 && cross(pts[idx[0]], pts[idx[1]], pts[idx[2]]) > 0.0 {
        out.push([idx[0], idx[1], idx[2]]);
    }
    out
}
