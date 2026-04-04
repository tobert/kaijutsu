// Stack Card Shader — 3D material for conversation stack cards.
//
// Features:
// - LOD text degradation (crisp -> blur -> colored bars as cards recede)
// - Holographic SDF edge glow with chromatic aberration and time shimmer
// - Back face rendering (dark grid with role-colored edge tint)

#import bevy_pbr::{
    forward_io::VertexOutput,
    mesh_view_bindings::globals,
}

@group(#{MATERIAL_BIND_GROUP}) @binding(0) var card_texture: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(1) var card_sampler: sampler;

struct StackCardUniforms {
    card_params: vec4<f32>,     // [opacity, lod_factor, render_mode, clip_y]
    glow_color: vec4<f32>,      // [r, g, b, a] role color
    glow_params: vec4<f32>,     // [glow_intensity, unused, unused, unused]
};

@group(#{MATERIAL_BIND_GROUP}) @binding(2) var<uniform> uniforms: StackCardUniforms;

// ── LOD Degradation ─────────────────────────────────────────────────

fn sample_blurred(uv: vec2<f32>, radius: f32) -> vec4<f32> {
    // 9-tap weighted box blur via textureSampleLevel (non-uniform flow safe)
    let dims = vec2<f32>(textureDimensions(card_texture, 0));
    let step = radius / dims;

    let c  = textureSampleLevel(card_texture, card_sampler, uv, 0.0) * 4.0;
    let n  = textureSampleLevel(card_texture, card_sampler, uv + vec2<f32>(0.0, -step.y), 0.0);
    let s  = textureSampleLevel(card_texture, card_sampler, uv + vec2<f32>(0.0,  step.y), 0.0);
    let e  = textureSampleLevel(card_texture, card_sampler, uv + vec2<f32>( step.x, 0.0), 0.0);
    let w  = textureSampleLevel(card_texture, card_sampler, uv + vec2<f32>(-step.x, 0.0), 0.0);
    let ne = textureSampleLevel(card_texture, card_sampler, uv + vec2<f32>( step.x, -step.y), 0.0);
    let nw = textureSampleLevel(card_texture, card_sampler, uv + vec2<f32>(-step.x, -step.y), 0.0);
    let se = textureSampleLevel(card_texture, card_sampler, uv + vec2<f32>( step.x,  step.y), 0.0);
    let sw = textureSampleLevel(card_texture, card_sampler, uv + vec2<f32>(-step.x,  step.y), 0.0);

    return (c + (n + s + e + w) * 2.0 + ne + nw + se + sw) / 16.0;
}

fn colored_bars(uv: vec2<f32>) -> vec4<f32> {
    // Abstract representation: horizontal color bands sampled from texture
    let band_count = 8.0;
    let band_index = floor(uv.y * band_count);
    let band_center_y = (band_index + 0.5) / band_count;
    let band_color = textureSampleLevel(card_texture, card_sampler,
                                         vec2<f32>(0.5, band_center_y), 0.0);

    // Gap between bars
    let within_band = fract(uv.y * band_count);
    let bar_mask = smoothstep(0.0, 0.08, within_band) *
                   smoothstep(1.0, 0.92, within_band);

    return vec4<f32>(band_color.rgb * 0.7, bar_mask * band_color.a);
}

fn sample_with_lod(uv: vec2<f32>, lod_factor: f32) -> vec4<f32> {
    // 0.0 = crisp, 0.1-0.5 = progressive blur, 0.5-0.85 = blur -> bars
    let blur_amount = smoothstep(0.1, 0.5, lod_factor);
    let bars_amount = smoothstep(0.5, 0.85, lod_factor);

    let crisp = textureSample(card_texture, card_sampler, uv);
    let blur_radius = mix(1.0, 6.0, blur_amount);
    let blurred = sample_blurred(uv, blur_radius);
    let bars = colored_bars(uv);

    // Blend: crisp -> blurred -> bars
    let after_blur = mix(crisp, blurred, blur_amount);
    return mix(after_blur, bars, bars_amount);
}

// ── Holographic Edge Glow ───────────────────────────────────────────

fn holographic_edge_glow(uv: vec2<f32>, glow_color: vec3<f32>,
                          glow_intensity: f32, time: f32) -> vec3<f32> {
    // SDF rounded-box edge distance
    let p = uv - 0.5;
    let half_size = vec2<f32>(0.5, 0.5);
    let corner_r = 0.02;
    let q = abs(p) - half_size + corner_r;
    let edge_d = min(max(q.x, q.y), 0.0) + length(max(q, vec2<f32>(0.0))) - corner_r;
    // edge_d: negative inside, 0 at edge, positive outside

    let glow_width = 0.06;
    let base_glow = exp(edge_d / glow_width) * glow_intensity;

    // Time-animated shimmer along edge perimeter
    let perimeter_t = atan2(p.y, p.x);
    let shimmer = 0.75 + 0.25 * sin(perimeter_t * 6.0 + time * 3.0);

    // Chromatic aberration: shift R/B along edge normal
    let edge_normal = normalize(max(q, vec2<f32>(0.001)));
    let ca_offset = 0.003 * base_glow;
    let r_shift = textureSampleLevel(card_texture, card_sampler,
                                      uv + edge_normal * ca_offset, 0.0).r;
    let g_center = textureSampleLevel(card_texture, card_sampler, uv, 0.0).g;
    let b_shift = textureSampleLevel(card_texture, card_sampler,
                                      uv - edge_normal * ca_offset, 0.0).b;
    let ca_color = vec3<f32>(r_shift, g_center, b_shift);

    // Combine: role-colored glow with shimmer + subtle CA
    let glow_rgb = glow_color * base_glow * shimmer;
    let ca_blend = base_glow * 0.3;

    return glow_rgb + (ca_color - vec3<f32>(0.5)) * ca_blend;
}

// ── Back Face ───────────────────────────────────────────────────────

fn back_face_pattern(uv: vec2<f32>, glow_color: vec4<f32>) -> vec4<f32> {
    let bg = vec3<f32>(0.03, 0.03, 0.04);

    // Grid pattern
    let grid_scale = 20.0;
    let grid_uv = fract(uv * grid_scale);
    let grid_line = smoothstep(0.02, 0.0, min(grid_uv.x, grid_uv.y))
                  + smoothstep(0.02, 0.0, min(1.0 - grid_uv.x, 1.0 - grid_uv.y));
    let grid = min(grid_line, 1.0) * 0.08;

    // Role-colored edge tint
    let edge_dist = min(min(uv.x, 1.0 - uv.x), min(uv.y, 1.0 - uv.y));
    let edge_tint = smoothstep(0.15, 0.0, edge_dist) * 0.25;

    let color = bg + vec3<f32>(grid) + glow_color.rgb * edge_tint;
    return vec4<f32>(color, 0.95);
}

// ── Gap Sparkle ────────────────────────────────────────────────────
// Animated twinkle dots inside a card-shaped outline.
// Marks where the reading card was pulled from the strip.

fn gap_sparkle(uv: vec2<f32>, glow_color: vec3<f32>, time: f32, opacity: f32) -> vec4<f32> {
    // SDF card outline
    let p = uv - 0.5;
    let half = vec2<f32>(0.48, 0.48);
    let corner = 0.02;
    let q = abs(p) - half + corner;
    let d = min(max(q.x, q.y), 0.0) + length(max(q, vec2<f32>(0.0))) - corner;

    // Thin glowing outline
    let outline = exp(-abs(d) / 0.012) * 0.35;

    // Sparkle grid — 5x5 cells, each with a twinkling dot
    let grid_size = 5.0;
    let cell = floor(uv * grid_size);
    let cell_uv = fract(uv * grid_size) - 0.5;

    // Pseudo-random per-cell values
    let h1 = fract(sin(dot(cell, vec2<f32>(127.1, 311.7))) * 43758.5453);
    let h2 = fract(sin(dot(cell, vec2<f32>(269.5, 183.3))) * 43758.5453);

    // Sharp twinkle: sin^4 with per-cell phase and speed
    let phase = h1 * 6.283 + time * (1.5 + h2 * 3.0);
    let brightness = pow(max(sin(phase), 0.0), 4.0);

    // Jittered sparkle position within cell
    let jitter = vec2<f32>(h1 - 0.5, h2 - 0.5) * 0.5;
    let dist = length(cell_uv - jitter);
    let point = smoothstep(0.18, 0.0, dist) * brightness;

    // Mask: only sparkle inside the card area
    let inside = smoothstep(0.005, -0.005, d);
    let sparkle = point * inside;

    let color = glow_color * (outline + sparkle * 1.2);
    let a = (outline * 0.5 + sparkle * 0.9) * opacity;

    return vec4<f32>(color, a);
}

// ── Main Fragment ───────────────────────────────────────────────────

@fragment
fn fragment(
    in: VertexOutput,
    @builtin(front_facing) is_front: bool,
) -> @location(0) vec4<f32> {
    let opacity = uniforms.card_params.x;
    let lod_factor = uniforms.card_params.y;
    let render_mode = uniforms.card_params.z;
    let clip_y = uniforms.card_params.w;
    let glow_color = uniforms.glow_color;
    let glow_intensity = uniforms.glow_params.x;
    let time = globals.time;

    // Clip fragments below the strip area (reading mode)
    if in.world_position.y < clip_y {
        discard;
    }

    // Gap sparkle mode — animated twinkle placeholder
    if render_mode > 0.5 {
        return gap_sparkle(in.uv, glow_color.rgb, time, opacity);
    }

    // Back face: dark grid with role-colored edge tint
    if !is_front {
        let back = back_face_pattern(in.uv, glow_color);
        return vec4<f32>(back.rgb, back.a * opacity);
    }

    // Front face: LOD-degraded texture + holographic glow
    let texture_color = sample_with_lod(in.uv, lod_factor);

    let edge_glow = holographic_edge_glow(
        in.uv, glow_color.rgb, glow_intensity, time
    );

    let final_rgb = texture_color.rgb + edge_glow;
    let edge_alpha = length(edge_glow) * 0.5;
    let final_a = max(texture_color.a, edge_alpha * glow_color.a) * opacity;

    return vec4<f32>(final_rgb, final_a);
}
