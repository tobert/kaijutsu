// Corner Shader with Flip Support
// One shader handles all four corners via flip_x/flip_y uniforms.
// Draws L-shaped bracket with inner highlight and diamond accent.
#import bevy_ui::ui_vertex_output::UiVertexOutput

@group(1) @binding(0) var<uniform> color: vec4<f32>;
@group(1) @binding(1) var<uniform> params: vec4<f32>;      // glow_radius, intensity, pulse_speed, bracket_length
@group(1) @binding(2) var<uniform> time: vec4<f32>;        // elapsed_time, _, _, _
@group(1) @binding(3) var<uniform> flip: vec4<f32>;        // flip_x, flip_y, _, _
@group(1) @binding(4) var<uniform> dimensions: vec4<f32>;  // width_px, height_px, corner_size, scale

@fragment
fn fragment(in: UiVertexOutput) -> @location(0) vec4<f32> {
    // Apply flip transformation - mirrors UV space for different corners
    var uv = in.uv;
    if (flip.x > 0.5) { uv.x = 1.0 - uv.x; }
    if (flip.y > 0.5) { uv.y = 1.0 - uv.y; }

    // Now uv is always oriented as if we're drawing top-left corner
    // uv (0,0) = outer corner, (1,1) = inner corner toward center

    let glow_radius = params.x;
    let glow_intensity = params.y;
    let pulse_speed = params.z;
    let bracket_length = params.w;  // 0-1, how far the L extends
    let elapsed = time.x;

    // Line parameters in UV space (0-1)
    let outer_margin = 0.04;      // Distance from UV edge to outer line
    let outer_thick = 0.05;       // Outer line thickness
    let inner_margin = 0.15;      // Distance from UV edge to inner line
    let inner_thick = 0.03;       // Inner line (thinner, highlight)
    let diamond_size = 0.12;      // Size of corner accent

    // Distance from edges
    let dist_left = uv.x;         // Distance from left edge
    let dist_top = uv.y;          // Distance from top edge

    var min_dist_outer = 999.0;
    var min_dist_inner = 999.0;
    var corner_accent = 0.0;

    // Check if within bracket region
    let bracket_extent = bracket_length;

    // === OUTER BRACKET (L-shape) ===
    // Vertical arm of the L (left side)
    if dist_left < outer_margin + outer_thick && dist_top < bracket_extent {
        let dist_to_line = abs(dist_left - outer_margin - outer_thick * 0.5);
        min_dist_outer = min(min_dist_outer, dist_to_line);
    }
    // Horizontal arm of the L (top side)
    if dist_top < outer_margin + outer_thick && dist_left < bracket_extent {
        let dist_to_line = abs(dist_top - outer_margin - outer_thick * 0.5);
        min_dist_outer = min(min_dist_outer, dist_to_line);
    }

    // === INNER HIGHLIGHT (shorter, offset inward) ===
    let inner_bracket = bracket_extent * 0.6;
    // Vertical arm
    if dist_left > inner_margin - inner_thick && dist_left < inner_margin + inner_thick && dist_top < inner_bracket {
        let dist_to_line = abs(dist_left - inner_margin);
        min_dist_inner = min(min_dist_inner, dist_to_line);
    }
    // Horizontal arm
    if dist_top > inner_margin - inner_thick && dist_top < inner_margin + inner_thick && dist_left < inner_bracket {
        let dist_to_line = abs(dist_top - inner_margin);
        min_dist_inner = min(min_dist_inner, dist_to_line);
    }

    // === CORNER DIAMOND ACCENT ===
    let diamond_center = vec2f(outer_margin + outer_thick + 0.08, outer_margin + outer_thick + 0.08);
    let diamond_uv = uv - diamond_center;
    let diamond_dist = abs(diamond_uv.x) + abs(diamond_uv.y);
    if diamond_dist < diamond_size {
        corner_accent = 1.0 - diamond_dist / diamond_size;
    }

    // Early exit if not near any element
    let near_something = min_dist_outer < outer_thick * 2.0 + glow_radius ||
                         min_dist_inner < inner_thick * 2.0 + glow_radius * 0.5 ||
                         corner_accent > 0.0;
    if !near_something {
        return vec4f(0.0, 0.0, 0.0, 0.0);
    }

    // Animated pulse
    let pulse = 0.7 + 0.3 * sin(elapsed * pulse_speed);

    // Outer line - main bracket
    let outer_line = (1.0 - smoothstep(0.0, outer_thick * 0.5, min_dist_outer)) * 0.9;
    let outer_glow = exp(-min_dist_outer * (8.0 / glow_radius)) * glow_intensity * 0.4 * pulse;

    // Inner line - bright highlight
    let inner_line = (1.0 - smoothstep(0.0, inner_thick * 0.5, min_dist_inner)) * 0.7;
    let inner_glow = exp(-min_dist_inner * (12.0 / glow_radius)) * glow_intensity * 0.3 * pulse;

    // Corner diamond accent
    let diamond = corner_accent * corner_accent * 0.8 * pulse;

    // Color palette - blend input color with cyberpunk palette
    let deep_purple = vec3f(0.3, 0.15, 0.5);
    let mid_purple = vec3f(0.45, 0.25, 0.65);
    let cyan_highlight = vec3f(0.3, 0.7, 0.9);
    let white_hot = vec3f(0.9, 0.85, 0.95);

    // Mix colors - use input color for tinting
    let tint = color.rgb;
    let outer_color = mix(deep_purple, tint, 0.5) * mix(vec3f(1.0), mid_purple, pulse * 0.3);
    let inner_color = mix(cyan_highlight, white_hot, inner_line * 0.6);
    let diamond_color = mix(tint * 0.8, cyan_highlight, 0.5);

    // Combine all elements
    let outer_contrib = outer_color * (outer_line + outer_glow);
    let inner_contrib = inner_color * (inner_line + inner_glow);
    let diamond_contrib = diamond_color * diamond;

    let final_color = outer_contrib + inner_contrib + diamond_contrib;

    // Alpha
    let alpha = (outer_line * 0.95 + outer_glow * 0.6 +
                 inner_line * 0.85 + inner_glow * 0.5 +
                 diamond * 0.9) * color.a;

    return vec4f(final_color, min(alpha, 1.0));
}
