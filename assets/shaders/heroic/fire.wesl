// Heroic view roadside edge fire (`docs/newui.md`, "Edge fire").
//
// Additive, double-sided quad standing along each road edge. Cut into one bar
// per beat cell locked to the lane's beat lines, with OKLCH rainbow hue
// traveling along the road and white root at the base. Applies distance fog.

import bevy_pbr::render::forward_io::VertexOutput;
import bevy_pbr::render::mesh_view_bindings::{globals, view};
import super::super::kaijutsu::color::{rainbow, apply_fog};

/// `[beat_len, beat_scroll_z, intensity, reduced_motion]`
@group(constants::MATERIAL_BIND_GROUP) @binding(0) var<uniform> params: vec4<f32>;
/// `[rainbow_lightness, chroma_max, road_length, chroma_min]`
@group(constants::MATERIAL_BIND_GROUP) @binding(1) var<uniform> ramp: vec4<f32>;
/// `[fog_color.rgb, fog_start]`
@group(constants::MATERIAL_BIND_GROUP) @binding(2) var<uniform> fog_color_start: vec4<f32>;
/// `[fog_end, _, _, _]`
@group(constants::MATERIAL_BIND_GROUP) @binding(3) var<uniform> fog_end_pad: vec4<f32>;
/// 16-band energy packed into 4 vec4s.
@group(constants::MATERIAL_BIND_GROUP) @binding(4) var<uniform> bands: array<vec4<f32>, 4>;

fn band_energy(idx: i32) -> f32 {
    let clamped = clamp(idx, 0, 15);
    let v = bands[clamped / 4];
    let rem = clamped % 4;
    if (rem == 0) { return v.x; }
    if (rem == 1) { return v.y; }
    if (rem == 2) { return v.z; }
    return v.w;
}

fn hash2(p: vec2<f32>) -> f32 {
    let h = dot(p, vec2<f32>(127.1, 311.7));
    return fract(sin(h) * 43758.5453123);
}

fn value_noise(p: vec2<f32>) -> f32 {
    let i = floor(p);
    let f = fract(p);
    let u = f * f * (3.0 - 2.0 * f);
    let a = hash2(i);
    let b = hash2(i + vec2<f32>(1.0, 0.0));
    let c = hash2(i + vec2<f32>(0.0, 1.0));
    let d = hash2(i + vec2<f32>(1.0, 1.0));
    return mix(mix(a, b, u.x), mix(c, d, u.x), u.y);
}

@fragment
fn fragment(in: VertexOutput) -> @location(0) vec4<f32> {
    let beat_len = max(params.x, 0.05);
    let scroll_z = params.y;
    let intensity = params.z;
    let frozen = params.w > 0.5;

    // Measure road_z from world-space -Z so left and right fire walls always
    // orient identically regardless of quad UV mirroring.
    let road_z = max(0.0, -in.world_position.z);
    let height_y = clamp(1.0 - in.uv.y, 0.0, 1.0);

    // One bar per beat cell, traveling toward the viewer with beat_scroll_z.
    let v_z = road_z + scroll_z;
    let cell = floor(v_z / beat_len);
    let cell_frac = fract(v_z / beat_len);

    // Triangle-wave band index so neighboring beats have similar heights.
    let b = abs(fract(cell / 30.0) * 2.0 - 1.0) * 15.0;
    var e: f32 = 0.0;
    for (var i: i32 = 0; i < 16; i = i + 1) {
        e = e + band_energy(i) * max(0.0, 1.0 - abs(f32(i) - b));
    }

    let h = min(0.95, (0.06 + e * 0.7) * (0.25 + intensity * 1.5));
    let anim_t = select(globals.time, 0.0, frozen);
    let n = value_noise(vec2<f32>(v_z * 3.0, height_y * 5.0 - anim_t * 3.0)) * 0.6
          + value_noise(vec2<f32>(v_z * 7.0, height_y * 11.0 - anim_t * 5.0)) * 0.4;

    var alpha = 1.0 - smoothstep(h * 0.4, h, height_y + (n - 0.5) * 0.7 * h);
    alpha = alpha * step(0.14, cell_frac);
    if (alpha < 0.01) {
        discard;
    }

    // Hue slides along road + height; chroma scales with bar energy e.
    let hue_t = fract(cell * 0.045 + height_y * 0.25 - anim_t * 0.05);
    let fire_chroma = mix(ramp.w, ramp.y, clamp(e, 0.0, 1.0));
    let flame_col = rainbow(hue_t, ramp.x, fire_chroma);
    let core = 1.0 - smoothstep(0.0, max(h * 0.22, 0.02), height_y);
    let col = mix(flame_col * 1.35, vec3<f32>(1.0, 1.0, 1.0), core * 0.55);

    // Apply HDR alpha scale once before additive blend (AlphaMode::Add uses src * 1.0).
    let hdr = col * (0.55 + intensity * 0.90) * alpha;
    let dist = distance(view.world_position, in.world_position.xyz);
    let fogged = apply_fog(hdr, fog_color_start.rgb * alpha, dist, fog_color_start.w, fog_end_pad.x);
    return vec4<f32>(fogged, 1.0);
}
