// Block Border Shader
// Single-node animated border for conversation blocks.
//
// Supports three border kinds:
//   0 = Full rectangle (rounded corners)
//   1 = Top accent line only
//   2 = Dashed rectangle
//
// And four animation modes:
//   0 = Static
//   1 = Chase (traveling light around perimeter)
//   2 = Pulse (sin-wave intensity for errors)
//   3 = Breathe (subtle amplitude for thinking)
//
// Uniforms:
//   color      - Border color (RGBA)
//   params     - [thickness_px, corner_radius_px, glow_radius, glow_intensity]
//   time       - [elapsed_time, ...]
//   mode       - [animation_mode, animation_speed, dash_count, border_kind]
//   dimensions - [width_px, height_px, ...]

#import bevy_ui::ui_vertex_output::UiVertexOutput

@group(1) @binding(0) var<uniform> color: vec4<f32>;
@group(1) @binding(1) var<uniform> params: vec4<f32>;
@group(1) @binding(2) var<uniform> time: vec4<f32>;
@group(1) @binding(3) var<uniform> mode: vec4<f32>;
@group(1) @binding(4) var<uniform> dimensions: vec4<f32>;

// Signed distance to a rounded rectangle centered at origin.
fn sdf_rounded_rect(p: vec2<f32>, half_size: vec2<f32>, radius: f32) -> f32 {
    let q = abs(p) - half_size + vec2<f32>(radius, radius);
    return length(max(q, vec2<f32>(0.0))) + min(max(q.x, q.y), 0.0) - radius;
}

// Position along perimeter (0-1), clockwise from top-left.
fn perimeter_position(uv: vec2<f32>) -> f32 {
    let d_top = uv.y;
    let d_bottom = 1.0 - uv.y;
    let d_left = uv.x;
    let d_right = 1.0 - uv.x;

    let min_dist = min(min(d_top, d_bottom), min(d_left, d_right));

    if min_dist == d_top {
        return uv.x * 0.25;
    } else if min_dist == d_right {
        return 0.25 + uv.y * 0.25;
    } else if min_dist == d_bottom {
        return 0.5 + (1.0 - uv.x) * 0.25;
    } else {
        return 0.75 + (1.0 - uv.y) * 0.25;
    }
}

// Wraparound distance on [0,1) circle.
fn perimeter_distance(a: f32, b: f32) -> f32 {
    let d1 = abs(a - b);
    return min(d1, 1.0 - d1);
}

@fragment
fn fragment(in: UiVertexOutput) -> @location(0) vec4<f32> {
    let uv = in.uv;
    let t = time.x;

    // Unpack uniforms
    let thickness_px = params.x;
    let corner_radius_px = params.y;
    let glow_radius = params.z;
    let glow_intensity = params.w;

    let animation_mode = mode.x;   // 0=static, 1=chase, 2=pulse, 3=breathe
    let animation_speed = mode.y;
    let dash_count = mode.z;
    let border_kind = mode.w;       // 0=full, 1=top_accent, 2=dashed

    let width_px = dimensions.x;
    let height_px = dimensions.y;

    // Convert pixel measurements to UV space
    let thickness_u = thickness_px / width_px;
    let thickness_v = thickness_px / height_px;
    let radius_u = corner_radius_px / width_px;
    let radius_v = corner_radius_px / height_px;

    // === Border Kind: Top Accent ===
    if border_kind > 0.5 && border_kind < 1.5 {
        // Just a line across the top
        let in_top = step(uv.y, thickness_v);
        let glow_falloff = exp(-uv.y / (glow_radius * 0.3)) * glow_intensity * 0.3;
        let intensity = in_top * 0.7 + glow_falloff;

        if intensity < 0.01 {
            return vec4<f32>(0.0, 0.0, 0.0, 0.0);
        }

        let alpha = min(intensity * color.a, 1.0);
        return vec4<f32>(color.rgb * intensity, alpha);
    }

    // === Full / Dashed border via rounded-rect SDF ===
    // Map UV to centered coordinates in pixel space
    let p = (uv - vec2<f32>(0.5, 0.5)) * vec2<f32>(width_px, height_px);
    let half_size = vec2<f32>(width_px * 0.5, height_px * 0.5);

    // SDF distance (negative inside, positive outside)
    let d = sdf_rounded_rect(p, half_size, corner_radius_px);

    // Distance from the border band: abs(d) == 0 at the edge, increases inward/outward
    let border_dist = abs(d) - thickness_px * 0.5;

    // Smooth border with 1px anti-aliasing
    let border_alpha = 1.0 - smoothstep(-1.0, 1.0, border_dist);

    // Only render near the edge
    if border_alpha < 0.005 && abs(d) > thickness_px + glow_radius * width_px * 0.3 {
        return vec4<f32>(0.0, 0.0, 0.0, 0.0);
    }

    // Glow: soft falloff outside the border
    let glow_falloff = exp(-max(border_dist, 0.0) / (glow_radius * width_px * 0.15)) * glow_intensity * 0.25;

    var intensity = border_alpha + glow_falloff;

    // === Dashed border ===
    if border_kind > 1.5 {
        let peri = perimeter_position(uv);
        let dash_phase = fract(peri * dash_count);
        let dash_mask = step(dash_phase, 0.5);
        intensity *= dash_mask;
    }

    // === Animation ===
    // Chase: traveling light
    if animation_mode > 0.5 && animation_mode < 1.5 {
        let peri = perimeter_position(uv);
        let chase_pos = fract(t * animation_speed);
        let chase_dist = perimeter_distance(peri, chase_pos);

        let chase_width = 0.12;
        let chase_bright = exp(-pow(chase_dist / chase_width, 2.0) * 10.0) * 1.5;
        intensity += chase_bright * border_alpha;
    }

    // Pulse: sin-wave intensity (error)
    if animation_mode > 1.5 && animation_mode < 2.5 {
        let pulse = 0.7 + 0.3 * sin(t * animation_speed * 3.14159);
        intensity *= pulse;
    }

    // Breathe: subtle amplitude (thinking)
    if animation_mode > 2.5 {
        let breath = 0.85 + 0.15 * sin(t * animation_speed);
        intensity *= breath;
    }

    if intensity < 0.01 {
        return vec4<f32>(0.0, 0.0, 0.0, 0.0);
    }

    let alpha = min(intensity * color.a, 1.0);
    return vec4<f32>(color.rgb * intensity, alpha);
}
