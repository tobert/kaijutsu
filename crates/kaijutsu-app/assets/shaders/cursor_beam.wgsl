// Cursor Shader - Wandering spirit
// A soft glowing orb that illuminates the edges as it passes
#import bevy_ui::ui_vertex_output::UiVertexOutput

@group(1) @binding(0) var<uniform> color: vec4<f32>;      // Base color (RGBA)
@group(1) @binding(1) var<uniform> params: vec4<f32>;     // x=orb_size, y=intensity, z=wander_speed, w=blink_rate
@group(1) @binding(2) var<uniform> time: vec4<f32>;       // x=elapsed_time, y=mode

@fragment
fn fragment(in: UiVertexOutput) -> @location(0) vec4<f32> {
    let uv = in.uv;
    let elapsed = time.x;
    let mode = time.y;

    let orb_size = max(params.x, 0.01); // Prevent division by zero
    let intensity = params.y;
    let wander_speed = params.z;
    let blink_rate = params.w;

    // Soft blink
    var blink = 1.0;
    if blink_rate > 0.0 {
        let phase = elapsed * blink_rate;
        blink = 0.6 + 0.4 * (0.5 + 0.5 * cos(phase * 6.28318));
    }

    // Wandering orb position - Lissajous-like motion
    let wander_x = 0.5 + 0.28 * sin(elapsed * wander_speed * 1.0);
    let wander_y = 0.5 + 0.28 * sin(elapsed * wander_speed * 1.3 + 1.0);
    let orb_center = vec2f(wander_x, wander_y);

    // Distance from wandering orb center
    let orb_dist = length(uv - orb_center);

    // Orb with bloom layers
    let orb_core = smoothstep(orb_size * 0.35, 0.0, orb_dist);
    let orb_bloom1 = exp(-orb_dist * orb_dist * (50.0 / orb_size)) * 0.6;
    let orb_bloom2 = exp(-orb_dist * orb_dist * (15.0 / orb_size)) * 0.35;
    let orb_bloom3 = exp(-orb_dist * orb_dist * (4.0 / orb_size)) * 0.15;

    let orb_alpha = (orb_core + orb_bloom1 + orb_bloom2 + orb_bloom3) * intensity;

    // Edge illumination - edges glow based on orb proximity
    // Much more dramatic falloff - far edges nearly invisible
    var edge_alpha = 0.0;
    let margin = 0.06;

    if mode < 0.5 {
        // LINE MODE: vertical line, glows when orb is near
        let line_x = 0.3;
        let dist_to_line = abs(uv.x - line_x);
        let orb_to_line = abs(orb_center.x - line_x);

        // Steep proximity falloff
        let proximity = exp(-orb_to_line * orb_to_line * 25.0);
        let y_proximity = exp(-(uv.y - orb_center.y) * (uv.y - orb_center.y) * 12.0);

        edge_alpha = exp(-dist_to_line * dist_to_line * 100.0) * proximity * y_proximity * 0.95;

    } else if mode < 1.5 {
        // BLOCK MODE: edges glow where orb is nearby
        let d_left = uv.x - margin;
        let d_right = (1.0 - margin) - uv.x;
        let d_top = uv.y - margin;
        let d_bottom = (1.0 - margin) - uv.y;

        // Orb distance to each edge
        let orb_d_left = orb_center.x - margin;
        let orb_d_right = (1.0 - margin) - orb_center.x;
        let orb_d_top = orb_center.y - margin;
        let orb_d_bottom = (1.0 - margin) - orb_center.y;

        // Steep proximity falloff - far edges almost invisible
        let falloff = 35.0;
        let spread = 8.0;

        // Left edge
        let left_prox = exp(-orb_d_left * orb_d_left * falloff);
        let left_y_prox = exp(-(uv.y - orb_center.y) * (uv.y - orb_center.y) * spread);
        let left_glow = exp(-d_left * d_left * 300.0) * left_prox * left_y_prox;

        // Right edge
        let right_prox = exp(-orb_d_right * orb_d_right * falloff);
        let right_y_prox = exp(-(uv.y - orb_center.y) * (uv.y - orb_center.y) * spread);
        let right_glow = exp(-d_right * d_right * 300.0) * right_prox * right_y_prox;

        // Top edge
        let top_prox = exp(-orb_d_top * orb_d_top * falloff);
        let top_x_prox = exp(-(uv.x - orb_center.x) * (uv.x - orb_center.x) * spread);
        let top_glow = exp(-d_top * d_top * 300.0) * top_prox * top_x_prox;

        // Bottom edge
        let bottom_prox = exp(-orb_d_bottom * orb_d_bottom * falloff);
        let bottom_x_prox = exp(-(uv.x - orb_center.x) * (uv.x - orb_center.x) * spread);
        let bottom_glow = exp(-d_bottom * d_bottom * 300.0) * bottom_prox * bottom_x_prox;

        edge_alpha = (left_glow + right_glow + top_glow + bottom_glow) * 1.0;

    } else {
        // UNDERLINE MODE: bottom line glows when orb is near
        let line_y = 0.85;
        let dist_to_line = abs(uv.y - line_y);
        let orb_to_line = abs(orb_center.y - line_y);

        let proximity = exp(-orb_to_line * orb_to_line * 25.0);
        let x_proximity = exp(-(uv.x - orb_center.x) * (uv.x - orb_center.x) * 10.0);

        edge_alpha = exp(-dist_to_line * dist_to_line * 150.0) * proximity * x_proximity * 0.95;
    }

    // Combine orb and edge illumination
    let total_alpha = (orb_alpha + edge_alpha) * blink;

    // Clamp alpha to prevent overbrightening (blooms are additive)
    let final_alpha = clamp(total_alpha * color.a, 0.0, 1.0);

    return vec4f(color.rgb, final_alpha);
}
