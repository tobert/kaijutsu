// Pulse Ring Shader
// Expanding ring/ripple effect for focus or notification
#import bevy_ui::ui_vertex_output::UiVertexOutput

// Separate bindings to match Bevy's AsBindGroup derive
@group(1) @binding(0) var<uniform> color: vec4<f32>;      // Ring color
@group(1) @binding(1) var<uniform> params: vec4<f32>;     // x=ring_count, y=ring_width, z=speed, w=max_radius
@group(1) @binding(2) var<uniform> time: vec4<f32>;       // x=elapsed_time, y=fade_start

@fragment
fn fragment(in: UiVertexOutput) -> @location(0) vec4<f32> {
    // Center UV
    let uv = in.uv * 2.0 - 1.0;
    let dist = length(uv);

    // Extract parameters
    let ring_count = params.x;
    let ring_width = params.y;
    let speed = params.z;
    let max_radius = params.w;
    let elapsed = time.x;
    let fade_start = time.y;

    var total_alpha = 0.0;

    // Draw multiple rings at different phases
    for (var i = 0.0; i < 4.0; i += 1.0) {
        if i >= ring_count {
            break;
        }

        // Stagger ring phases
        let phase = i / ring_count;
        let ring_time = fract(elapsed * speed + phase);

        // Ring expands from center
        let ring_radius = ring_time * max_radius;

        // Distance to ring
        let ring_dist = abs(dist - ring_radius);

        // Ring shape with soft edges
        let ring = smoothstep(ring_width, ring_width * 0.2, ring_dist);

        // Fade out as ring expands
        let fade = 1.0 - smoothstep(fade_start * max_radius, max_radius, ring_radius);

        // Also fade in at the start
        let fade_in = smoothstep(0.0, 0.1 * max_radius, ring_radius);

        total_alpha += ring * fade * fade_in;
    }

    // Clamp total alpha
    total_alpha = min(total_alpha, 1.0);

    return vec4f(color.rgb, total_alpha * color.a);
}
