// Heroic view spectrogram hills (`docs/newui.md`, "Spectrogram hills").
//
// Custom vertex + fragment material on the two 12x32 flank grid meshes.
// The vertex shader displaces height from the 32-row x 16-band eighth-note
// peak history with smooth phase interpolation and pins the inner roadside edge
// (`d = 0`) cleanly to the ground. The fragment shader fills with `hill_fill`
// and draws thin OKLCH rainbow wireframe grid lines with distance fog.

import bevy_pbr::render::mesh_functions::{get_world_from_local, mesh_position_local_to_world};
import bevy_pbr::render::view_transformations::position_world_to_clip;
import bevy_pbr::render::mesh_view_bindings::view;
import super::super::kaijutsu::color::{rainbow, apply_fog};

struct Vertex {
    @builtin(instance_index) instance_index: u32,
    @location(0) position: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) uv: vec2<f32>,
}

struct HillVertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) world_position: vec4<f32>,
    @location(1) uv: vec2<f32>,
    @location(2) height_norm: f32,
}

/// `[gain, phase, intensity, reduced_motion]`
@group(constants::MATERIAL_BIND_GROUP) @binding(0) var<uniform> params: vec4<f32>;
/// `[rainbow_lightness, rainbow_chroma, grid_cols, grid_rows]`
@group(constants::MATERIAL_BIND_GROUP) @binding(1) var<uniform> grid: vec4<f32>;
/// `[hill_fill.rgb, fog_start]`
@group(constants::MATERIAL_BIND_GROUP) @binding(2) var<uniform> fill_fog_start: vec4<f32>;
/// `[fog_color.rgb, fog_end]`
@group(constants::MATERIAL_BIND_GROUP) @binding(3) var<uniform> fog_color_end: vec4<f32>;
/// 32 history rows x 16 bands = 128 vec4<f32> (4 vec4 per row).
@group(constants::MATERIAL_BIND_GROUP) @binding(4) var<uniform> history: array<vec4<f32>, 128>;

fn sample_history(row_idx: i32, band_idx: i32) -> f32 {
    let r = clamp(row_idx, 0, 31);
    let b = clamp(band_idx, 0, 15);
    let v = history[r * 4 + (b / 4)];
    let rem = b % 4;
    if (rem == 0) { return v.x; }
    if (rem == 1) { return v.y; }
    if (rem == 2) { return v.z; }
    return v.w;
}

fn ridge_noise(x: f32, z: f32) -> f32 {
    return 0.5 + 0.5 * sin(x * 7.3 + z * 5.1) * cos(x * 3.7 - z * 9.2);
}

@vertex
fn vertex(in: Vertex) -> HillVertexOutput {
    var out: HillVertexOutput;

    // `in.uv.x` is d in [0, 1] outward from the roadside inner edge.
    // `in.uv.y` is age fraction in [0, 1] from strike line (0) to far end (1).
    let d = clamp(in.uv.x, 0.0, 1.0);
    let age_f = clamp(in.uv.y, 0.0, 1.0) * 30.0;
    let age0 = i32(floor(age_f));
    let age1 = min(age0 + 1, 31);

    let band_f = d * 15.0;
    let b0 = i32(floor(band_f));
    let b1 = min(b0 + 1, 15);
    let b_frac = fract(band_f);

    let phase = select(clamp(params.y, 0.0, 1.0), 0.0, params.w > 0.5);
    let v00 = sample_history(age0, b0);
    let v01 = sample_history(age0, b1);
    let v10 = sample_history(age1, b0);
    let v11 = sample_history(age1, b1);
    let cur = mix(v00, v01, b_frac);
    let prev = mix(v10, v11, b_frac);
    let value = mix(cur, prev, phase);

    // Pinned at roadside (`d = 0`): envelope = 0 at edge, grows smoothly outward.
    let envelope = select(d, d * d / 0.25, d < 0.25);
    let base_ridge = d * d * (1.0 + ridge_noise(d, in.uv.y) * 2.2);
    let disp = base_ridge + value * envelope * 8.5 * params.x;

    var local_pos = in.position;
    local_pos.y = local_pos.y + disp;

    let world_from_local = get_world_from_local(in.instance_index);
    out.world_position = mesh_position_local_to_world(world_from_local, vec4<f32>(local_pos, 1.0));
    out.position = position_world_to_clip(out.world_position.xyz);
    out.uv = in.uv;
    out.height_norm = clamp(disp / 6.0, 0.0, 1.0);
    return out;
}

@fragment
fn fragment(in: HillVertexOutput) -> @location(0) vec4<f32> {
    let cell = vec2<f32>(in.uv.x * grid.z, in.uv.y * grid.w);
    let fw = max(fwidth(cell), vec2<f32>(0.001));
    let dist_edge = min(fract(cell), 1.0 - fract(cell)) / fw;
    let line = 1.0 - min(min(dist_edge.x, dist_edge.y), 1.0);

    let hue_t = fract(in.uv.x * 0.75 + in.uv.y * 0.35 + in.height_norm * 0.2);
    let wire_col = rainbow(hue_t, grid.x, grid.y) * (0.45 + 0.5 * params.z);
    let base_col = mix(fill_fog_start.rgb, wire_col, line);

    let dist = distance(view.world_position, in.world_position.xyz);
    let fogged = apply_fog(base_col, fog_color_end.rgb, dist, fill_fog_start.w, fog_color_end.w);
    return vec4<f32>(fogged, 1.0);
}
