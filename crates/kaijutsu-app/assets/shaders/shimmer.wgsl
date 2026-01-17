// Shimmer Shader
// Sparkle/twinkle effect overlay for active states
//
// Attribution:
// - Sparkle technique inspired by shadplay fast_dots.wgsl
// - Hash functions from Dave_Hoskins: https://www.shadertoy.com/view/4djSRW
//
#import bevy_ui::ui_vertex_output::UiVertexOutput

// Separate bindings to match Bevy's AsBindGroup derive
@group(1) @binding(0) var<uniform> color: vec4<f32>;      // Sparkle color
@group(1) @binding(1) var<uniform> params: vec4<f32>;     // x=density, y=speed, z=brightness, w=size
@group(1) @binding(2) var<uniform> time: vec4<f32>;       // x=elapsed_time

// Hash function for pseudo-random sparkle positions
fn hash21(p: vec2f) -> f32 {
    return fract(sin(dot(p, vec2f(127.1, 311.7))) * 43758.5453);
}

fn hash22(p: vec2f) -> vec2f {
    var p3 = fract(vec3f(p.xyx) * vec3f(0.1031, 0.103, 0.0973));
    p3 += dot(p3, p3.yzx + 33.33);
    return fract((p3.xx + p3.yz) * p3.zy);
}

@fragment
fn fragment(in: UiVertexOutput) -> @location(0) vec4<f32> {
    let uv = in.uv;

    // Extract parameters
    let density = params.x;
    let speed = params.y;
    let brightness = params.z;
    let size = params.w;
    let elapsed = time.x;

    // Create grid for sparkle positions
    let grid_uv = uv * density;
    let grid_id = floor(grid_uv);
    let grid_fract = fract(grid_uv) - 0.5;  // Center in cell

    // Random offset within cell
    let rand = hash22(grid_id);
    let offset = (rand - 0.5) * 0.8;  // Keep sparkles mostly centered

    // Distance to sparkle center
    let sparkle_pos = grid_fract - offset;
    let dist = length(sparkle_pos);

    // Random phase for each sparkle
    let phase = hash21(grid_id) * 6.28318;
    let time_offset = hash21(grid_id + 100.0) * 10.0;

    // Twinkle animation - varies per sparkle
    let twinkle = sin(elapsed * speed + phase + time_offset);
    let twinkle_factor = max(0.0, twinkle);  // Only positive values

    // Sparkle shape - sharp bright center with soft falloff
    let sparkle_radius = size * (0.5 + 0.5 * twinkle_factor);
    let sparkle = smoothstep(sparkle_radius, sparkle_radius * 0.1, dist);

    // Add cross/star shape
    let cross_x = smoothstep(sparkle_radius * 2.0, 0.0, abs(sparkle_pos.x)) *
                  smoothstep(sparkle_radius * 0.5, 0.0, abs(sparkle_pos.y));
    let cross_y = smoothstep(sparkle_radius * 2.0, 0.0, abs(sparkle_pos.y)) *
                  smoothstep(sparkle_radius * 0.5, 0.0, abs(sparkle_pos.x));
    let cross = (cross_x + cross_y) * 0.3;

    // Combine sparkle and cross
    let intensity = (sparkle + cross) * twinkle_factor * brightness;

    // Some sparkles are dimmer (variety)
    let brightness_var = 0.5 + 0.5 * hash21(grid_id + 200.0);

    return vec4f(color.rgb * intensity * brightness_var, intensity * color.a);
}
