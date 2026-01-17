// Edge Shader with Tiling/Stretching Support
// Renders glowing energy lines between corners
#import bevy_ui::ui_vertex_output::UiVertexOutput

@group(1) @binding(0) var<uniform> color: vec4<f32>;
@group(1) @binding(1) var<uniform> params: vec4<f32>;      // glow_radius, intensity, pulse_speed, _
@group(1) @binding(2) var<uniform> time: vec4<f32>;        // elapsed_time, _, _, _
@group(1) @binding(3) var<uniform> tile_info: vec4<f32>;   // tile_size, mode (0=stretch, 1=tile), length_px, thickness_px
@group(1) @binding(4) var<uniform> orientation: vec4<f32>; // x=is_vertical (0=horizontal, 1=vertical)

@fragment
fn fragment(in: UiVertexOutput) -> @location(0) vec4<f32> {
    var uv = in.uv;

    // For vertical edges, swap coordinates so we can use the same logic
    let is_vertical = orientation.x > 0.5;
    if is_vertical {
        uv = vec2f(uv.y, uv.x);
    }

    let tile_size = tile_info.x;
    let tile_mode = tile_info.y;
    let length_px = tile_info.z;
    let thickness_px = tile_info.w;

    let glow_radius = params.x;
    let glow_intensity = params.y;
    let pulse_speed = params.z;
    let elapsed = time.x;

    // For tiling: repeat the pattern
    var pattern_uv = uv;
    if tile_mode > 0.5 && tile_size > 0.0 {
        let tiles = length_px / tile_size;
        pattern_uv.x = fract(uv.x * tiles);
    }

    // Distance from center line (y=0.5 is center)
    let center_dist = abs(uv.y - 0.5) * 2.0; // 0 at center, 1 at edges

    // Core energy line
    let line_width = 0.15;
    let line_intensity = 1.0 - smoothstep(0.0, line_width, center_dist);

    // Outer glow
    let glow_falloff = exp(-center_dist * (3.0 / glow_radius)) * glow_intensity * 0.5;

    // Animated shimmer along the edge
    let shimmer_speed = pulse_speed * 2.0;
    let shimmer = sin(pattern_uv.x * 30.0 - elapsed * shimmer_speed) * 0.5 + 0.5;

    // Pulse animation
    let pulse = 0.75 + 0.25 * sin(elapsed * pulse_speed);

    // Combine intensities
    let core = line_intensity * (0.6 + shimmer * 0.4) * pulse;
    let glow = glow_falloff * pulse;
    let total_intensity = core + glow;

    if total_intensity < 0.02 {
        return vec4f(0.0, 0.0, 0.0, 0.0);
    }

    // Color mixing
    let base = color.rgb;
    let bright = mix(base, vec3f(1.0), 0.3);
    let final_color = mix(base * glow, bright * core, core / (core + glow + 0.001));

    let alpha = min(total_intensity * color.a, 1.0);

    return vec4f(final_color, alpha);
}
