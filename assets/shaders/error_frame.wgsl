// Error Frame Shader
// Displays a red dashed border to indicate missing frame configuration
#import bevy_ui::ui_vertex_output::UiVertexOutput

@group(1) @binding(0) var<uniform> color: vec4<f32>;
@group(1) @binding(1) var<uniform> time: vec4<f32>;        // elapsed_time, _, _, _

@fragment
fn fragment(in: UiVertexOutput) -> @location(0) vec4<f32> {
    let uv = in.uv;
    let elapsed = time.x;

    // Border thickness (in UV space)
    let border_width = 0.04;

    // Check if we're in the border region
    let dist_left = uv.x;
    let dist_right = 1.0 - uv.x;
    let dist_top = uv.y;
    let dist_bottom = 1.0 - uv.y;

    let min_dist = min(min(dist_left, dist_right), min(dist_top, dist_bottom));
    let in_border = min_dist < border_width;

    if !in_border {
        return vec4f(0.0, 0.0, 0.0, 0.0);
    }

    // Determine position along the border for dashing
    var border_pos: f32;
    if dist_left < border_width || dist_right < border_width {
        // Vertical edges - use y position
        border_pos = uv.y;
    } else {
        // Horizontal edges - use x position
        border_pos = uv.x;
    }

    // Animated dashed pattern
    let dash_length = 0.08;
    let dash_speed = 0.5;
    let dash_phase = fract((border_pos + elapsed * dash_speed) / dash_length);

    // Dash visibility (50% dash, 50% gap)
    let is_dash = dash_phase < 0.5;

    if !is_dash {
        return vec4f(0.0, 0.0, 0.0, 0.0);
    }

    // Pulsing intensity
    let pulse = 0.7 + 0.3 * sin(elapsed * 3.0);

    // Error red color
    let error_color = color.rgb * pulse;

    return vec4f(error_color, color.a * 0.9);
}
