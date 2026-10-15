//! CPU tessellation of rings, blob shadows and dashed paths into one vertex
//! list. The fragment shaders in `overlay.wgsl` do the shaping, so the
//! geometry only has to cover the area each overlay can touch.

use std::f32::consts::{PI, TAU};

use meerora_geom::{Vec2, Vec3};

use crate::{BlobShadow, DashedPath, Ring};

/// Shadows sit this far above their centre to avoid z-fighting with the floor.
pub(crate) const SHADOW_LIFT: f32 = 0.25;
/// Paths sit a little higher than shadows so they stay on top.
pub(crate) const PATH_LIFT: f32 = 0.5;
/// Segments of a full ring. Partial arcs use a share of it.
const RING_SEGMENTS: f32 = 96.0;
/// Path strips are this much wider than the dashes, room for the edge fade.
const PATH_STRIP_SCALE: f32 = 1.3;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub(crate) struct OverlayVertex {
    pub position: [f32; 3],
    pub local: [f32; 2],
    pub params: [f32; 4],
    pub extra: [f32; 4],
    pub color: [f32; 4],
}

/// True for finite and infinite positive values, false for zero, negatives and NaN.
fn positive(x: f32) -> bool {
    x > 0.0
}

/// Width of the soft halo outside the ring core.
pub(crate) fn ring_halo(ring: &Ring) -> f32 {
    ring.width.max(0.0) * 1.6 + 2.0
}

/// Appends a ring band as a triangle list.
pub(crate) fn push_ring(out: &mut Vec<OverlayVertex>, ring: &Ring) {
    if !positive(ring.radius) || !positive(ring.intensity) || !positive(ring.arc_len) {
        return;
    }
    let half_width = ring.width.max(0.0) * 0.5;
    let halo = ring_halo(ring);
    let reach = half_width + halo;
    let inner = (ring.radius - reach).max(0.0);
    let full = ring.arc_len >= TAU;

    // Partial arcs get round caps, so the band extends past both ends.
    let (a0, span) = if full {
        (0.0, TAU)
    } else {
        let cap = (reach / inner.max(ring.radius * 0.25)).min(PI);
        let span = ring.arc_len + 2.0 * cap;
        if span >= TAU { (0.0, TAU) } else { (ring.arc_start - cap, span) }
    };
    let segments = ((span / TAU) * RING_SEGMENTS).ceil().max(8.0) as usize;
    let step = span / segments as f32;
    // Push the outer edge out so the polygon contains the true circle.
    let outer = (ring.radius + reach) / (step * 0.5).cos();

    let params = [ring.radius, half_width, halo, ring.arc_start];
    let extra = [if full { TAU } else { ring.arc_len }, 0.0, 0.0, 0.0];
    let color = [ring.color[0], ring.color[1], ring.color[2], ring.intensity];
    let vertex = |rho: f32, a: f32| {
        let (s, c) = a.sin_cos();
        let local = Vec2::new(rho * c, rho * s);
        let position = ring.center + Vec3::new(local.x, 0.0, -local.y);
        OverlayVertex { position: position.to_array(), local: local.to_array(), params, extra, color }
    };
    // Shared edges use identical vertices, so the band has no cracks.
    let edge = |i: usize| {
        let a = if span >= TAU && i == segments { a0 } else { a0 + step * i as f32 };
        (vertex(inner, a), vertex(outer, a))
    };
    let mut prev = edge(0);
    for i in 1..=segments {
        let next = edge(i);
        let ((ia, oa), (ib, ob)) = (prev, next);
        out.extend_from_slice(&[ia, oa, ob, ia, ob, ib]);
        prev = next;
    }
}

/// Appends a blob shadow quad.
pub(crate) fn push_shadow(out: &mut Vec<OverlayVertex>, shadow: &BlobShadow) {
    if !positive(shadow.radius) || !positive(shadow.opacity) {
        return;
    }
    let c = shadow.center + Vec3::Y * SHADOW_LIFT;
    let r = shadow.radius;
    let color = [0.0, 0.0, 0.0, shadow.opacity.min(1.0)];
    let vertex = |u: f32, v: f32| OverlayVertex {
        position: (c + Vec3::new(u * r, 0.0, v * r)).to_array(),
        local: [u, v],
        params: [0.0; 4],
        extra: [0.0; 4],
        color,
    };
    let (a, b, cc, d) = (vertex(-1.0, -1.0), vertex(1.0, -1.0), vertex(1.0, 1.0), vertex(-1.0, 1.0));
    out.extend_from_slice(&[a, b, cc, a, cc, d]);
}

/// Appends a dashed path as a strip with mitred joins. Dashes are measured by
/// arc length in the xz plane.
pub(crate) fn push_path(out: &mut Vec<OverlayVertex>, path: &DashedPath) {
    if !positive(path.width) || !positive(path.opacity) {
        return;
    }
    // Drop points that do not move in the ground plane.
    let mut points: Vec<Vec3> = Vec::with_capacity(path.points.len());
    for &p in &path.points {
        if points.last().is_none_or(|q: &Vec3| Vec2::new(p.x - q.x, p.z - q.z).length() > 1e-3) {
            points.push(p);
        }
    }
    if points.len() < 2 {
        return;
    }

    let half_width = path.width * 0.5;
    let strip = half_width * PATH_STRIP_SCALE;
    let params = [path.dash.max(0.0), path.gap.max(0.0), half_width, 0.0];
    let color = [path.color[0], path.color[1], path.color[2], path.opacity.min(1.0)];
    let side_of = |a: Vec3, b: Vec3| {
        let d = Vec2::new(b.x - a.x, b.z - a.z).normalize();
        Vec3::new(-d.y, 0.0, d.x)
    };

    // Offset direction and arc length at every point.
    let mut rails: Vec<(Vec3, Vec3, f32)> = Vec::with_capacity(points.len());
    let mut s = 0.0;
    for i in 0..points.len() {
        if i > 0 {
            s += Vec2::new(points[i].x - points[i - 1].x, points[i].z - points[i - 1].z).length();
        }
        let offset = if i == 0 {
            side_of(points[0], points[1])
        } else if i == points.len() - 1 {
            side_of(points[i - 1], points[i])
        } else {
            let prev = side_of(points[i - 1], points[i]);
            let next = side_of(points[i], points[i + 1]);
            let miter = (prev + next).normalize_or(prev);
            miter / miter.dot(prev).max(0.5)
        };
        rails.push((points[i] + Vec3::Y * PATH_LIFT, offset * strip, s));
    }

    let vertex = |p: Vec3, side: f32, s: f32| OverlayVertex {
        position: p.to_array(),
        local: [s, side * PATH_STRIP_SCALE],
        params,
        extra: [0.0; 4],
        color,
    };
    for pair in rails.windows(2) {
        let (p0, o0, s0) = pair[0];
        let (p1, o1, s1) = pair[1];
        let (l0, r0) = (vertex(p0 + o0, 1.0, s0), vertex(p0 - o0, -1.0, s0));
        let (l1, r1) = (vertex(p1 + o1, 1.0, s1), vertex(p1 - o1, -1.0, s1));
        out.extend_from_slice(&[r0, r1, l1, r0, l1, l0]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ring(arc_len: f32) -> Ring {
        Ring {
            center: Vec3::new(0.0, 100.0, 0.0),
            radius: 30.0,
            width: 3.0,
            color: [1.0, 0.5, 0.1],
            intensity: 1.0,
            arc_start: 0.0,
            arc_len,
        }
    }

    #[test]
    fn full_ring_covers_its_reach() {
        let mut out = Vec::new();
        push_ring(&mut out, &ring(TAU));
        assert_eq!(out.len() % 3, 0);
        let r = ring(TAU);
        let reach = r.radius + r.width * 0.5 + ring_halo(&r);
        for k in 0..64 {
            let a = k as f32 / 64.0 * TAU;
            let p = Vec2::new(a.cos(), a.sin()) * reach * 0.999;
            let inside = out.as_chunks::<3>().0.iter().any(|t| {
                let [a, b, c] = [t[0].local, t[1].local, t[2].local].map(Vec2::from);
                let s = |p0: Vec2, p1: Vec2| (p1 - p0).perp_dot(p - p0);
                let (d0, d1, d2) = (s(a, b), s(b, c), s(c, a));
                (d0 >= 0.0 && d1 >= 0.0 && d2 >= 0.0) || (d0 <= 0.0 && d1 <= 0.0 && d2 <= 0.0)
            });
            assert!(inside, "point at angle {a} not covered");
        }
    }

    #[test]
    fn angle_maps_to_minus_z() {
        let mut out = Vec::new();
        push_ring(&mut out, &ring(TAU));
        for v in &out {
            let p = Vec3::from(v.position) - ring(TAU).center;
            assert!((p.x - v.local[0]).abs() < 1e-4 && (p.z + v.local[1]).abs() < 1e-4);
        }
    }

    #[test]
    fn half_ring_uses_fewer_triangles() {
        let (mut full, mut half) = (Vec::new(), Vec::new());
        push_ring(&mut full, &ring(TAU));
        push_ring(&mut half, &ring(PI));
        assert!(half.len() < full.len());
    }

    #[test]
    fn path_tracks_arc_length() {
        let path = DashedPath {
            points: vec![Vec3::ZERO, Vec3::new(100.0, 0.0, 0.0), Vec3::new(100.0, 0.0, 50.0), Vec3::new(100.0, 0.0, 50.0)],
            width: 4.0,
            dash: 8.0,
            gap: 6.0,
            color: [0.5, 0.4, 1.0],
            opacity: 0.8,
        };
        let mut out = Vec::new();
        push_path(&mut out, &path);
        assert_eq!(out.len(), 12);
        let max_s = out.iter().map(|v| v.local[0]).fold(0.0, f32::max);
        assert!((max_s - 150.0).abs() < 1e-3);
    }

    #[test]
    fn degenerate_inputs_add_nothing() {
        let mut out = Vec::new();
        push_ring(&mut out, &Ring { radius: 0.0, ..ring(TAU) });
        push_shadow(&mut out, &BlobShadow { center: Vec3::ZERO, radius: 10.0, opacity: 0.0 });
        push_path(&mut out, &DashedPath { points: vec![Vec3::ZERO], width: 2.0, dash: 1.0, gap: 1.0, color: [1.0; 3], opacity: 1.0 });
        assert!(out.is_empty());
    }
}
