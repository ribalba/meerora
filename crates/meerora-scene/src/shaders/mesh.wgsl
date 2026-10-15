// Opaque flat-shaded meshes, drawn instanced. Needs common.wgsl.

struct MeshIn {
    @location(0) position: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) color: vec3<f32>,
    @location(3) emissive: f32,
    @location(4) model_0: vec4<f32>,
    @location(5) model_1: vec4<f32>,
    @location(6) model_2: vec4<f32>,
    @location(7) model_3: vec4<f32>,
    @location(8) normal_0: vec4<f32>,
    @location(9) normal_1: vec4<f32>,
    @location(10) normal_2: vec4<f32>,
    // x: brightness, y: emissive scale.
    @location(11) params: vec4<f32>,
}

struct MeshOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) world: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) color: vec3<f32>,
    @location(3) emissive: f32,
    @location(4) brightness: f32,
}

@vertex
fn vs_mesh(in: MeshIn) -> MeshOut {
    let model = mat4x4<f32>(in.model_0, in.model_1, in.model_2, in.model_3);
    let normal_matrix = mat3x3<f32>(in.normal_0.xyz, in.normal_1.xyz, in.normal_2.xyz);
    let world = model * vec4<f32>(in.position, 1.0);
    var out: MeshOut;
    out.clip = globals.view_proj * world;
    out.world = world.xyz;
    out.normal = normal_matrix * in.normal;
    out.color = in.color;
    out.emissive = in.emissive * in.params.y;
    out.brightness = in.params.x;
    return out;
}

@fragment
fn fs_mesh(in: MeshOut) -> @location(0) vec4<f32> {
    let n = normalize(in.normal);

    // Hemisphere ambient, ground colour facing down and sky colour facing up.
    var light = mix(globals.ambient_ground.rgb, globals.ambient_sky.rgb, n.y * 0.5 + 0.5);

    // Soft sun. Wrapped Lambert keeps the shadow side from going fully black.
    let sun_wrap = 0.5;
    light += globals.sun_color.rgb * max((dot(n, globals.to_sun.xyz) + sun_wrap) / (1.0 + sun_wrap), 0.0);

    let count = min(u32(globals.params.x), MAX_POINT_LIGHTS);
    for (var i = 0u; i < count; i++) {
        let l = globals.lights[i];
        let to_light = l.position_radius.xyz - in.world;
        let dist = length(to_light);
        let x = saturate(1.0 - (dist * dist) / (l.position_radius.w * l.position_radius.w));
        let falloff = x * x;
        let point_wrap = 0.25;
        let facing = max((dot(n, to_light / max(dist, 0.001)) + point_wrap) / (1.0 + point_wrap), 0.0);
        light += l.color.rgb * falloff * facing;
    }

    let lit = in.color * light * in.brightness;
    let glow = in.color * in.emissive;
    return vec4<f32>(encode(lit + glow), 1.0);
}
