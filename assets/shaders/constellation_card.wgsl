// Constellation Card Shader
// Rectangular card node for constellation context visualization.
//
// Renders:
//   - Dark fill (#0c0c1e)
//   - Colored border with soft outer glow (color from agent/provider)
//   - Activity indicator dot (top-right corner)
//   - Focused state: brighter border, larger glow
//
// Uniforms:
//   color      - Border/agent color (RGBA)
//   params     - [thickness_px, corner_radius_px, glow_radius, glow_intensity]
//   time       - [elapsed_time, ...]
//   mode       - [activity_dot_r, activity_dot_g, activity_dot_b, reserved]
//   dimensions - [width_px, height_px, opacity, focused(0/1)]

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

@fragment
fn fragment(in: UiVertexOutput) -> @location(0) vec4<f32> {
    let uv = in.uv;
    let t = time.x;

    // Unpack uniforms
    let thickness_px = params.x;
    let corner_radius_px = params.y;
    let glow_radius = params.z;
    let glow_intensity = params.w;

    let width_px = dimensions.x;
    let height_px = dimensions.y;
    let opacity = dimensions.z;
    let focused = dimensions.w;

    // Activity dot color from mode uniform
    let dot_color = vec3<f32>(mode.x, mode.y, mode.z);

    // Map UV to centered pixel coordinates
    let p = (uv - vec2<f32>(0.5, 0.5)) * vec2<f32>(width_px, height_px);
    let half_size = vec2<f32>(width_px * 0.5, height_px * 0.5);

    // SDF distance (negative inside, positive outside)
    let d = sdf_rounded_rect(p, half_size, corner_radius_px);

    // === Dark fill ===
    // Inside the rectangle: dark background (#0c0c1e in sRGB → linear)
    // srgb_to_linear(x) ≈ pow(x, 2.2) for values > 0.04045
    let fill_color = vec3<f32>(0.003, 0.003, 0.012);
    let fill_alpha = 1.0 - smoothstep(-1.0, 0.0, d);

    // === Border ===
    let border_dist = abs(d) - thickness_px * 0.5;
    let border_alpha = 1.0 - smoothstep(-1.0, 1.0, border_dist);

    // Focused pulse: subtle breathing on the border
    let focus_pulse = select(1.0, 0.85 + 0.15 * sin(t * 2.0), focused > 0.5);

    // Border color with focus brightness boost
    let focus_boost = select(1.0, 1.3, focused > 0.5);
    let border_col = color.rgb * focus_boost * focus_pulse;

    // === Outer glow (ONLY outside the shape) ===
    // d > 0 means outside; glow falls off with distance from edge
    let outside_mask = step(0.0, d); // 1.0 outside, 0.0 inside
    let glow_spread = glow_radius * min(width_px, height_px) * 0.15;
    let glow_falloff = exp(-d / max(glow_spread, 1.0)) * glow_intensity * outside_mask;
    let focus_glow_mult = select(1.0, 1.8, focused > 0.5);
    let glow = glow_falloff * focus_glow_mult;

    // === Activity indicator dot (top-right corner) ===
    let dot_radius = 4.0; // pixels
    let dot_margin = 8.0; // pixels from corner
    let dot_center = vec2<f32>(
        half_size.x - dot_margin - dot_radius,
        -(half_size.y - dot_margin - dot_radius)
    );
    let dot_dist = length(p - dot_center);
    let dot_alpha = 1.0 - smoothstep(dot_radius - 1.0, dot_radius + 1.0, dot_dist);
    // Dot glow
    let dot_glow = exp(-max(dot_dist - dot_radius, 0.0) / 6.0) * 0.4;
    let dot_has_color = step(0.01, dot_color.r + dot_color.g + dot_color.b);

    // === Compose layers ===
    // Start with fill (dark interior)
    var out_color = fill_color * fill_alpha;
    var out_alpha = fill_alpha * 0.92;

    // Outer glow (only outside the shape, does not brighten interior)
    let glow_contrib = color.rgb * glow * 0.6;
    out_color = out_color + glow_contrib;
    out_alpha = max(out_alpha, glow * 0.5);

    // Border on top (mix so it replaces fill at edge)
    out_color = mix(out_color, border_col, border_alpha * color.a);
    out_alpha = max(out_alpha, border_alpha * color.a);

    // Activity dot (only if it has a color set)
    let total_dot = (dot_alpha + dot_glow) * dot_has_color;
    out_color = mix(out_color, dot_color, total_dot);
    out_alpha = max(out_alpha, total_dot);

    // Apply overall opacity
    out_alpha *= opacity;

    // Early discard for fully transparent pixels
    if out_alpha < 0.005 {
        discard;
    }

    return vec4<f32>(out_color, out_alpha);
}
