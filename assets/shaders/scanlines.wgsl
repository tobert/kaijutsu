// Scanlines Shader
// Subtle CRT/cyberpunk scanline overlay
#import bevy_ui::ui_vertex_output::UiVertexOutput

// Separate bindings to match Bevy's AsBindGroup derive
@group(1) @binding(0) var<uniform> tint: vec4<f32>;       // Color tint
@group(1) @binding(1) var<uniform> params1: vec4<f32>;    // x=line_count, y=line_intensity, z=scroll_speed, w=flicker
@group(1) @binding(2) var<uniform> params2: vec4<f32>;    // x=noise_amount, y=curvature, z=unused, w=unused
@group(1) @binding(3) var<uniform> time: vec4<f32>;       // x=elapsed_time

// Simple noise
fn hash21(p: vec2f) -> f32 {
    return fract(sin(dot(p, vec2f(127.1, 311.7))) * 43758.5453);
}

@fragment
fn fragment(in: UiVertexOutput) -> @location(0) vec4<f32> {
    var uv = in.uv;

    // Extract parameters
    let line_count = params1.x;
    let line_intensity = params1.y;
    let scroll_speed = params1.z;
    let flicker_amount = params1.w;
    let noise_amount = params2.x;
    let curvature = params2.y;
    let elapsed = time.x;

    // Optional CRT curvature
    if curvature > 0.0 {
        let centered = uv * 2.0 - 1.0;
        let curved = centered + centered * dot(centered, centered) * curvature;
        uv = curved * 0.5 + 0.5;

        // Vignette from curvature
        if uv.x < 0.0 || uv.x > 1.0 || uv.y < 0.0 || uv.y > 1.0 {
            return vec4f(0.0);
        }
    }

    // Scrolling scanlines
    let scroll_y = uv.y + elapsed * scroll_speed;
    let scanline = 0.5 + 0.5 * sin(scroll_y * line_count * 3.14159 * 2.0);
    let scanline_factor = 1.0 - line_intensity * (1.0 - scanline);

    // Interlace effect (every other frame slightly offset)
    let interlace = 0.5 + 0.5 * sin(elapsed * 60.0);  // 60hz flicker
    let interlace_offset = interlace * 0.5 / line_count;
    let scanline2 = 0.5 + 0.5 * sin((scroll_y + interlace_offset) * line_count * 3.14159 * 2.0);
    let combined_scanline = mix(scanline_factor, 1.0 - line_intensity * (1.0 - scanline2), 0.5);

    // Screen flicker
    let flicker = 1.0 - flicker_amount * (0.5 + 0.5 * sin(elapsed * 8.0));

    // Static noise
    let noise = hash21(uv * 1000.0 + elapsed * 100.0);
    let noise_factor = 1.0 - noise_amount * noise;

    // Combine effects
    let effect = combined_scanline * flicker * noise_factor;

    return vec4f(tint.rgb * effect, tint.a * (1.0 - effect * 0.1));
}
