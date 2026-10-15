// Shared by every pipeline. Prepended to mesh.wgsl and overlay.wgsl.

const TAU: f32 = 6.283185307179586;
const MAX_POINT_LIGHTS: u32 = 16u;

struct Light {
    // xyz position, w radius where the light reaches zero.
    position_radius: vec4<f32>,
    // rgb colour already multiplied by intensity.
    color: vec4<f32>,
}

struct Globals {
    view_proj: mat4x4<f32>,
    ambient_sky: vec4<f32>,
    ambient_ground: vec4<f32>,
    // Unit vector from the scene towards the sun.
    to_sun: vec4<f32>,
    // rgb colour already multiplied by intensity.
    sun_color: vec4<f32>,
    // x: number of point lights, y: 1 when the target is not sRGB and the
    // shader has to encode the output itself.
    params: vec4<f32>,
    lights: array<Light, MAX_POINT_LIGHTS>,
}

@group(0) @binding(0) var<uniform> globals: Globals;

// Identity up to the knee, then a smooth shoulder towards 1. Keeps the
// palette intact for surfaces lit to about their base colour and only rolls
// off highlights, glows and overlapping lights.
fn tone_map(c: vec3<f32>) -> vec3<f32> {
    let knee = 0.8;
    let span = 1.0 - knee;
    let over = max(c - vec3<f32>(knee), vec3<f32>(0.0));
    let rolled = vec3<f32>(knee) + span * (vec3<f32>(1.0) - exp(-over / span));
    return select(c, rolled, c > vec3<f32>(knee));
}

fn linear_to_srgb(c: vec3<f32>) -> vec3<f32> {
    let lo = c * 12.92;
    let hi = 1.055 * pow(c, vec3<f32>(1.0 / 2.4)) - vec3<f32>(0.055);
    return select(hi, lo, c <= vec3<f32>(0.0031308));
}

// Final colour for the render target. sRGB targets convert in hardware.
fn encode(c: vec3<f32>) -> vec3<f32> {
    let mapped = clamp(tone_map(c), vec3<f32>(0.0), vec3<f32>(1.0));
    if globals.params.y > 0.5 {
        return linear_to_srgb(mapped);
    }
    return mapped;
}
