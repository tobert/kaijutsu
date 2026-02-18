// Drift Arc Shader
// Quadratic Bezier curve with glow, animated particle flow, and endpoint emphasis.
// Replaces straight connection lines with curved arcs for constellation drift visualization.
//
// The control point is computed from endpoints: perpendicular to the midpoint
// at curve_amount × distance. Curve direction is consistent (always bows left
// relative to from→to direction).
//
// Uniforms:
//   color      - Arc color (RGBA)
//   params     - [glow_width, intensity, flow_speed, curve_amount]
//   time       - [elapsed_time, activity_level, ...]
//   endpoints  - [x0, y0, x1, y1] normalized 0-1 relative to bounding box
//   dimensions - [width_px, height_px, aspect, falloff]

#import bevy_ui::ui_vertex_output::UiVertexOutput

@group(1) @binding(0) var<uniform> color: vec4<f32>;
@group(1) @binding(1) var<uniform> params: vec4<f32>;
@group(1) @binding(2) var<uniform> time: vec4<f32>;
@group(1) @binding(3) var<uniform> endpoints: vec4<f32>;
@group(1) @binding(4) var<uniform> dimensions: vec4<f32>;

// Evaluate quadratic Bezier at parameter t
fn bezier(p0: vec2<f32>, p1: vec2<f32>, p2: vec2<f32>, t: f32) -> vec2<f32> {
    let mt = 1.0 - t;
    return mt * mt * p0 + 2.0 * mt * t * p1 + t * t * p2;
}

// Find minimum distance from point to quadratic Bezier via sampling
// Returns vec2(min_distance, parameter_t_at_closest)
fn bezier_dist(p: vec2<f32>, b0: vec2<f32>, b1: vec2<f32>, b2: vec2<f32>) -> vec2<f32> {
    var min_dist = 1e10;
    var min_t = 0.0;

    // Coarse pass: 16 samples
    for (var i = 0; i <= 16; i++) {
        let t = f32(i) / 16.0;
        let bp = bezier(b0, b1, b2, t);
        let d = length(p - bp);
        if d < min_dist {
            min_dist = d;
            min_t = t;
        }
    }

    // Refine: 8 samples around the coarse minimum
    let step = 1.0 / 16.0;
    let t_lo = max(min_t - step, 0.0);
    let t_hi = min(min_t + step, 1.0);
    for (var i = 0; i <= 8; i++) {
        let t = t_lo + (t_hi - t_lo) * f32(i) / 8.0;
        let bp = bezier(b0, b1, b2, t);
        let d = length(p - bp);
        if d < min_dist {
            min_dist = d;
            min_t = t;
        }
    }

    return vec2<f32>(min_dist, min_t);
}

@fragment
fn fragment(in: UiVertexOutput) -> @location(0) vec4<f32> {
    let glow_width = params.x;
    let intensity = params.y;
    let flow_speed = params.z;
    let curve_amount = params.w;
    let elapsed = time.x;
    let activity = time.y;
    let aspect = dimensions.z;
    let falloff = dimensions.w;

    // Aspect-corrected UV and endpoints
    let uv = vec2<f32>(in.uv.x * aspect, in.uv.y);
    let p0 = vec2<f32>(endpoints.x * aspect, endpoints.y);
    let p1 = vec2<f32>(endpoints.z * aspect, endpoints.w);

    // Compute quadratic Bezier control point:
    // perpendicular to midpoint, offset by curve_amount × distance
    let mid = (p0 + p1) * 0.5;
    let dir = p1 - p0;
    let dist_endpoints = length(dir);
    // Perpendicular: rotate 90° left (consistent curve direction)
    let perp = vec2<f32>(-dir.y, dir.x);
    let perp_norm = perp / max(dist_endpoints, 0.001);
    let ctrl = mid + perp_norm * dist_endpoints * curve_amount;

    // Distance to Bezier curve
    let result = bezier_dist(uv, p0, ctrl, p1);
    let dist = result.x;
    let t = result.y;

    // Core arc with soft edges
    let core = smoothstep(glow_width * 0.12, 0.0, dist);

    // Outer glow with falloff
    let glow = exp(-dist * dist * falloff * 50.0);

    // Animated particle flow along curve
    let particle_count = 4.0;
    let flow_pos = fract(t * particle_count - elapsed * flow_speed);
    let flow_pulse = smoothstep(0.0, 0.2, flow_pos) * smoothstep(0.5, 0.2, flow_pos);
    let flow_effect = flow_pulse * glow * activity;

    // Endpoint glow (brighter near nodes)
    let endpoint_glow = (1.0 - t) * (1.0 - t) + t * t;
    let endpoint_effect = endpoint_glow * 0.3;

    // Combine
    let base_alpha = core * 0.8 + glow * 0.4 * intensity;
    let animated_alpha = base_alpha + flow_effect * 0.5 + endpoint_effect * glow;
    let final_alpha = animated_alpha * (0.3 + activity * 0.7);

    if final_alpha * color.a < 0.003 {
        discard;
    }

    return vec4<f32>(color.rgb, final_alpha * color.a);
}
