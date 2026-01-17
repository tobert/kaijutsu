// Ornate Corner Frame Shader
// Inspired by cyberpunk anime interfaces - double lines with decorative corners
#import bevy_ui::ui_vertex_output::UiVertexOutput

@group(1) @binding(0) var<uniform> color: vec4<f32>;
@group(1) @binding(1) var<uniform> params: vec4<f32>;     // x=glow_radius, y=intensity, z=pulse_speed, w=bracket_length
@group(1) @binding(2) var<uniform> time: vec4<f32>;       // x=elapsed_time

@fragment
fn fragment(in: UiVertexOutput) -> @location(0) vec4<f32> {
    let uv = in.uv * 2.0 - 1.0;  // -1 to 1 centered

    let glow_radius = params.x;
    let glow_intensity = params.y;
    let pulse_speed = params.z;
    let bracket_length = params.w;
    let elapsed = time.x;

    // Distances from edges
    let dist_left = uv.x + 1.0;
    let dist_right = 1.0 - uv.x;
    let dist_top = 1.0 - uv.y;
    let dist_bottom = uv.y + 1.0;

    // Line parameters
    let outer_margin = 0.015;      // Distance from UV edge to outer line
    let outer_thick = 0.012;       // Outer line thickness
    let inner_margin = 0.045;      // Distance from UV edge to inner line
    let inner_thick = 0.006;       // Inner line (thinner, highlight)
    let corner_size = 0.08;        // Size of decorative corner diamond

    var min_dist_outer = 999.0;
    var min_dist_inner = 999.0;
    var corner_accent = 0.0;

    // Check each corner region
    let in_left = dist_left < bracket_length;
    let in_right = dist_right < bracket_length;
    let in_top = dist_top < bracket_length;
    let in_bottom = dist_bottom < bracket_length;

    // === TOP-LEFT CORNER ===
    if in_left && in_top {
        // Outer bracket
        if dist_top < bracket_length * 0.9 {
            min_dist_outer = min(min_dist_outer, abs(dist_left - outer_margin - outer_thick * 0.5));
        }
        if dist_left < bracket_length * 0.9 {
            min_dist_outer = min(min_dist_outer, abs(dist_top - outer_margin - outer_thick * 0.5));
        }
        // Inner highlight
        if dist_top < bracket_length * 0.7 {
            min_dist_inner = min(min_dist_inner, abs(dist_left - inner_margin - inner_thick * 0.5));
        }
        if dist_left < bracket_length * 0.7 {
            min_dist_inner = min(min_dist_inner, abs(dist_top - inner_margin - inner_thick * 0.5));
        }
        // Corner diamond accent
        let corner_uv = vec2f(dist_left - outer_margin - outer_thick, dist_top - outer_margin - outer_thick);
        let diamond_dist = abs(corner_uv.x) + abs(corner_uv.y);
        if diamond_dist < corner_size {
            corner_accent = max(corner_accent, 1.0 - diamond_dist / corner_size);
        }
    }

    // === TOP-RIGHT CORNER ===
    if in_right && in_top {
        if dist_top < bracket_length * 0.9 {
            min_dist_outer = min(min_dist_outer, abs(dist_right - outer_margin - outer_thick * 0.5));
        }
        if dist_right < bracket_length * 0.9 {
            min_dist_outer = min(min_dist_outer, abs(dist_top - outer_margin - outer_thick * 0.5));
        }
        if dist_top < bracket_length * 0.7 {
            min_dist_inner = min(min_dist_inner, abs(dist_right - inner_margin - inner_thick * 0.5));
        }
        if dist_right < bracket_length * 0.7 {
            min_dist_inner = min(min_dist_inner, abs(dist_top - inner_margin - inner_thick * 0.5));
        }
        let corner_uv = vec2f(dist_right - outer_margin - outer_thick, dist_top - outer_margin - outer_thick);
        let diamond_dist = abs(corner_uv.x) + abs(corner_uv.y);
        if diamond_dist < corner_size {
            corner_accent = max(corner_accent, 1.0 - diamond_dist / corner_size);
        }
    }

    // === BOTTOM-LEFT CORNER ===
    if in_left && in_bottom {
        if dist_bottom < bracket_length * 0.9 {
            min_dist_outer = min(min_dist_outer, abs(dist_left - outer_margin - outer_thick * 0.5));
        }
        if dist_left < bracket_length * 0.9 {
            min_dist_outer = min(min_dist_outer, abs(dist_bottom - outer_margin - outer_thick * 0.5));
        }
        if dist_bottom < bracket_length * 0.7 {
            min_dist_inner = min(min_dist_inner, abs(dist_left - inner_margin - inner_thick * 0.5));
        }
        if dist_left < bracket_length * 0.7 {
            min_dist_inner = min(min_dist_inner, abs(dist_bottom - inner_margin - inner_thick * 0.5));
        }
        let corner_uv = vec2f(dist_left - outer_margin - outer_thick, dist_bottom - outer_margin - outer_thick);
        let diamond_dist = abs(corner_uv.x) + abs(corner_uv.y);
        if diamond_dist < corner_size {
            corner_accent = max(corner_accent, 1.0 - diamond_dist / corner_size);
        }
    }

    // === BOTTOM-RIGHT CORNER ===
    if in_right && in_bottom {
        if dist_bottom < bracket_length * 0.9 {
            min_dist_outer = min(min_dist_outer, abs(dist_right - outer_margin - outer_thick * 0.5));
        }
        if dist_right < bracket_length * 0.9 {
            min_dist_outer = min(min_dist_outer, abs(dist_bottom - outer_margin - outer_thick * 0.5));
        }
        if dist_bottom < bracket_length * 0.7 {
            min_dist_inner = min(min_dist_inner, abs(dist_right - inner_margin - inner_thick * 0.5));
        }
        if dist_right < bracket_length * 0.7 {
            min_dist_inner = min(min_dist_inner, abs(dist_bottom - inner_margin - inner_thick * 0.5));
        }
        let corner_uv = vec2f(dist_right - outer_margin - outer_thick, dist_bottom - outer_margin - outer_thick);
        let diamond_dist = abs(corner_uv.x) + abs(corner_uv.y);
        if diamond_dist < corner_size {
            corner_accent = max(corner_accent, 1.0 - diamond_dist / corner_size);
        }
    }

    // Early exit if not near any element
    let near_something = min_dist_outer < outer_thick + glow_radius * 3.0 ||
                         min_dist_inner < inner_thick + glow_radius * 2.0 ||
                         corner_accent > 0.0;
    if !near_something {
        return vec4f(0.0, 0.0, 0.0, 0.0);
    }

    // Animated pulse
    let pulse = 0.7 + 0.3 * sin(elapsed * pulse_speed);

    // Outer line - main bracket
    let outer_line = (1.0 - smoothstep(0.0, outer_thick, min_dist_outer)) * 0.8;
    let outer_glow = exp(-min_dist_outer * (2.5 / glow_radius)) * glow_intensity * 0.4 * pulse;

    // Inner line - bright highlight
    let inner_line = (1.0 - smoothstep(0.0, inner_thick, min_dist_inner)) * 0.6;
    let inner_glow = exp(-min_dist_inner * (4.0 / glow_radius)) * glow_intensity * 0.3 * pulse;

    // Corner diamond accent
    let diamond = corner_accent * corner_accent * 0.7 * pulse;

    // Color palette - deep purple base, cyan inner highlight
    let deep_purple = vec3f(0.3, 0.15, 0.5);
    let mid_purple = vec3f(0.45, 0.25, 0.65);
    let cyan_highlight = vec3f(0.3, 0.7, 0.9);
    let white_hot = vec3f(0.8, 0.75, 0.95);

    // Mix colors
    let outer_color = mix(deep_purple, mid_purple, pulse * 0.5);
    let inner_color = mix(cyan_highlight, white_hot, inner_line * 0.5);
    let diamond_color = mix(mid_purple, cyan_highlight, 0.6);

    // Combine all elements
    let outer_contrib = outer_color * (outer_line + outer_glow);
    let inner_contrib = inner_color * (inner_line + inner_glow);
    let diamond_contrib = diamond_color * diamond;

    // Blend with input color for mode-based tinting
    let base_result = outer_contrib + inner_contrib + diamond_contrib;
    let tinted = mix(base_result, color.rgb * 0.8, 0.25);

    // Alpha
    let alpha = (outer_line * 0.9 + outer_glow * 0.5 +
                 inner_line * 0.8 + inner_glow * 0.4 +
                 diamond * 0.9) * color.a;

    return vec4f(tinted, min(alpha, 1.0));
}
