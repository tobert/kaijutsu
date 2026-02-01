// HUD Panel Shader
// Rectangular panel with edge glow effect for HUD widgets
//
// Uniforms:
//   color        - Base panel color (RGBA)
//   glow_color   - Edge glow color (RGBA)
//   params       - [glow_intensity, border_radius, pulse_speed, _reserved]
//   time         - [elapsed_time, ...]

#import bevy_ui::ui_vertex_output::UiVertexOutput

@group(1) @binding(0) var<uniform> color: vec4<f32>;
@group(1) @binding(1) var<uniform> glow_color: vec4<f32>;
@group(1) @binding(2) var<uniform> params: vec4<f32>;
@group(1) @binding(3) var<uniform> time: vec4<f32>;

@fragment
fn fragment(in: UiVertexOutput) -> @location(0) vec4<f32> {
    let uv = in.uv;
    let t = time.x;

    // Unpack parameters
    let glow_intensity = params.x;
    let border_radius = params.y;
    let pulse_speed = params.z;

    // Distance from edges (0 at edge, 0.5 at center)
    let edge_dist_x = min(uv.x, 1.0 - uv.x);
    let edge_dist_y = min(uv.y, 1.0 - uv.y);
    let edge_dist = min(edge_dist_x, edge_dist_y);

    // Animated pulse
    let pulse = 0.85 + 0.15 * sin(t * pulse_speed);

    // Edge glow - brighter at edges, fading inward
    let glow_falloff = 0.12;  // How far inward glow extends
    let glow = smoothstep(glow_falloff, 0.0, edge_dist) * glow_intensity * pulse;

    // Corner glow - brighter in corners
    let corner_dist = length(vec2<f32>(0.5 - edge_dist_x, 0.5 - edge_dist_y) * 2.0);
    let corner_glow = smoothstep(0.7, 1.0, corner_dist) * glow_intensity * 0.3 * pulse;

    // Inner gradient - subtle vertical gradient for depth
    let inner_gradient = mix(1.0, 0.85, uv.y);

    // Base panel with gradient
    var final_color = color.rgb * inner_gradient;

    // Add edge glow
    let total_glow = glow + corner_glow;
    final_color = mix(final_color, glow_color.rgb, total_glow);

    // Add subtle inner highlight at top edge
    let top_highlight = smoothstep(0.05, 0.0, uv.y) * 0.15 * glow_intensity;
    final_color = final_color + glow_color.rgb * top_highlight;

    // Alpha: base panel alpha, boost slightly at edges for glow
    let alpha = color.a + total_glow * glow_color.a * 0.5;

    return vec4f(final_color, min(alpha, 1.0));
}
