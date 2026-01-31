// Connection Line Shader
// Glowing line effect for constellation connections
// Renders between two points with animated glow and distance falloff
#import bevy_ui::ui_vertex_output::UiVertexOutput

// Separate bindings to match Bevy's AsBindGroup derive
@group(1) @binding(0) var<uniform> color: vec4<f32>;      // Line color (RGBA)
@group(1) @binding(1) var<uniform> params: vec4<f32>;     // x=glow_width, y=intensity, z=flow_speed, w=unused
@group(1) @binding(2) var<uniform> time: vec4<f32>;       // x=elapsed_time, y=activity_level (0-1), z=unused, w=unused
@group(1) @binding(3) var<uniform> endpoints: vec4<f32>;  // x0, y0, x1, y1 (normalized 0-1)
@group(1) @binding(4) var<uniform> dimensions: vec4<f32>; // x=width, y=height, z=aspect (w/h), w=falloff

@fragment
fn fragment(in: UiVertexOutput) -> @location(0) vec4<f32> {
    // Extract parameters
    let glow_width = params.x;
    let intensity = params.y;
    let flow_speed = params.z;
    let elapsed = time.x;
    let activity = time.y;
    let aspect = dimensions.z;  // width / height
    let falloff = dimensions.w;

    // Correct UV for aspect ratio to ensure circular glow (not elliptical)
    // Scale X by aspect ratio so distance calculations are in square space
    let uv = vec2f(in.uv.x * aspect, in.uv.y);

    // Line endpoints corrected for aspect ratio
    let p0 = vec2f(endpoints.x * aspect, endpoints.y);
    let p1 = vec2f(endpoints.z * aspect, endpoints.w);

    // Line direction and length (in corrected space)
    let line_dir = p1 - p0;
    let line_len = length(line_dir);
    let line_norm = line_dir / max(line_len, 0.001);

    // Project UV onto line
    let to_uv = uv - p0;
    let t = clamp(dot(to_uv, line_norm) / line_len, 0.0, 1.0);
    let closest_point = p0 + line_dir * t;

    // Distance from UV to line (now aspect-corrected)
    let dist = length(uv - closest_point);

    // Core line with soft edges
    let core = smoothstep(glow_width * 0.1, 0.0, dist);

    // Outer glow with falloff
    let glow = exp(-dist * dist * falloff * 50.0);

    // Animated flow along line (energy particles)
    let flow_pos = fract(t * 3.0 - elapsed * flow_speed);
    let flow_pulse = smoothstep(0.0, 0.3, flow_pos) * smoothstep(0.6, 0.3, flow_pos);
    let flow_effect = flow_pulse * glow * activity;

    // Endpoint glow (brighter at nodes)
    let endpoint_glow = (1.0 - t) * (1.0 - t) + t * t;
    let endpoint_effect = endpoint_glow * 0.3;

    // Combine effects
    let base_alpha = core * 0.8 + glow * 0.4 * intensity;
    let animated_alpha = base_alpha + flow_effect * 0.5 + endpoint_effect * glow;

    // Activity-based intensity boost
    let final_alpha = animated_alpha * (0.3 + activity * 0.7);

    return vec4f(color.rgb, final_alpha * color.a);
}
