// Rings, blob shadows and dashed paths. Needs common.wgsl. The meaning of
// `local`, `params` and `extra` depends on the fragment entry point.

struct OverlayIn {
    @location(0) position: vec3<f32>,
    @location(1) local: vec2<f32>,
    @location(2) params: vec4<f32>,
    @location(3) extra: vec4<f32>,
    @location(4) color: vec4<f32>,
}

struct OverlayOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) local: vec2<f32>,
    @location(1) @interpolate(flat) params: vec4<f32>,
    @location(2) @interpolate(flat) extra: vec4<f32>,
    @location(3) @interpolate(flat) color: vec4<f32>,
}

@vertex
fn vs_overlay(in: OverlayIn) -> OverlayOut {
    var out: OverlayOut;
    out.clip = globals.view_proj * vec4<f32>(in.position, 1.0);
    out.local = in.local;
    out.params = in.params;
    out.extra = in.extra;
    out.color = in.color;
    return out;
}

const RING_HALO_GAIN: f32 = 0.6;

// local: position in the ring plane relative to the centre, (cos, sin) axes.
// params: radius, half width, halo width, arc start. extra.x: arc length.
// color: rgb, intensity. Premultiplied. The core covers what is behind it in
// the ring colour, the halo adds light on top.
@fragment
fn fs_ring(in: OverlayOut) -> @location(0) vec4<f32> {
    let radius = in.params.x;
    let half_width = in.params.y;
    let halo = in.params.z;
    let start = in.params.w;
    let len = in.extra.x;
    let p = in.local;

    var d = abs(length(p) - radius);
    if len < TAU {
        let a = atan2(p.y, p.x) - start;
        let rel = a - TAU * floor(a / TAU);
        if rel > len {
            let e0 = radius * vec2<f32>(cos(start), sin(start));
            let e1 = radius * vec2<f32>(cos(start + len), sin(start + len));
            d = min(length(p - e0), length(p - e1));
        }
    }

    let aa = max(fwidth(d), 0.0001);
    // Keep the core at least about a pixel wide so thin rings do not vanish
    // where the view foreshortens them.
    let core_half = max(half_width, aa * 0.75);
    let core = 1.0 - smoothstep(core_half - aa, core_half + aa, d);
    let t = saturate((d - half_width) / halo);
    let glow = (1.0 - t) * (1.0 - t) * (1.0 - core);
    let c = in.color.rgb * in.color.a * (core + glow * RING_HALO_GAIN);
    return vec4<f32>(encode(c), core * saturate(in.color.a));
}

// local: position relative to the centre divided by the radius.
// color: rgb, opacity. Alpha blended.
@fragment
fn fs_shadow(in: OverlayOut) -> @location(0) vec4<f32> {
    let k = 1.0 - smoothstep(0.45, 1.0, length(in.local));
    return vec4<f32>(encode(in.color.rgb), in.color.a * k);
}

// local: x arc length along the path, y side in units of the half width. The
// strip is a bit wider than the dashes so their edges can fade out.
// params: dash, gap, half width. color: rgb, opacity. Alpha blended.
@fragment
fn fs_path(in: OverlayOut) -> @location(0) vec4<f32> {
    let dash = in.params.x;
    let period = max(dash + in.params.y, 0.0001);
    let half_width = in.params.z;

    // Distance from the centre of the nearest dash, measured along the path.
    var x = in.local.x - dash * 0.5;
    x = x - period * floor(x / period);
    if x > period * 0.5 {
        x = x - period;
    }
    // Round caps make each dash a capsule of length `dash` and radius `half_width`.
    let along = max(abs(x) - max(dash * 0.5 - half_width, 0.0), 0.0);
    let across = abs(in.local.y) * half_width;
    let d = length(vec2<f32>(along, across));
    let aa = max(fwidth(d), 0.0001);
    let cover = 1.0 - smoothstep(half_width - aa * 0.5, half_width + aa * 0.5, d);
    return vec4<f32>(encode(in.color.rgb), in.color.a * cover);
}
