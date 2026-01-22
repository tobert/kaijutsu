// Chasing Border Shader
// Single-node animated border with traveling neon light effect
//
// The chase light travels around the perimeter clockwise:
// Top edge (0.0-0.25) → Right edge (0.25-0.5) → Bottom edge (0.5-0.75) → Left edge (0.75-1.0)
//
// Uniforms:
//   color        - Base border color (RGBA)
//   params       - [border_thickness_px, glow_radius, glow_intensity, chase_speed]
//   time         - [elapsed_time, ...]
//   chase        - [chase_width, chase_intensity, chase_tail_length, _]
//   chase_color  - Color for the chase highlight (hot pink by default)

#import bevy_ui::ui_vertex_output::UiVertexOutput

@group(1) @binding(0) var<uniform> color: vec4<f32>;
@group(1) @binding(1) var<uniform> params: vec4<f32>;
@group(1) @binding(2) var<uniform> time: vec4<f32>;
@group(1) @binding(3) var<uniform> chase: vec4<f32>;
@group(1) @binding(4) var<uniform> chase_color: vec4<f32>;

// Calculate position along perimeter (0-1) from UV coordinates
// Returns (perimeter_position, distance_from_edge)
fn perimeter_position(uv: vec2<f32>) -> vec2<f32> {
    // Distances from each edge
    let d_top = uv.y;
    let d_bottom = 1.0 - uv.y;
    let d_left = uv.x;
    let d_right = 1.0 - uv.x;

    // Find minimum distance to any edge
    let min_dist = min(min(d_top, d_bottom), min(d_left, d_right));

    // Determine which edge we're closest to and calculate perimeter position
    // Top: 0.0 - 0.25 (left to right)
    // Right: 0.25 - 0.5 (top to bottom)
    // Bottom: 0.5 - 0.75 (right to left)
    // Left: 0.75 - 1.0 (bottom to top)

    var pos = 0.0;

    if min_dist == d_top {
        // Top edge: position based on x (0 at left, 0.25 at right)
        pos = uv.x * 0.25;
    } else if min_dist == d_right {
        // Right edge: position based on y (0.25 at top, 0.5 at bottom)
        pos = 0.25 + uv.y * 0.25;
    } else if min_dist == d_bottom {
        // Bottom edge: position based on x (0.5 at right, 0.75 at left)
        pos = 0.5 + (1.0 - uv.x) * 0.25;
    } else {
        // Left edge: position based on y (0.75 at bottom, 1.0 at top)
        pos = 0.75 + (1.0 - uv.y) * 0.25;
    }

    return vec2<f32>(pos, min_dist);
}

// Smooth distance along perimeter (handles wraparound at 0/1)
fn perimeter_distance(a: f32, b: f32) -> f32 {
    let d1 = abs(a - b);
    let d2 = 1.0 - d1;  // Wraparound distance
    return min(d1, d2);
}

// HSV to RGB conversion for color cycling
fn hsv_to_rgb(h: f32, s: f32, v: f32) -> vec3<f32> {
    let c = v * s;
    let hp = h * 6.0;
    let x = c * (1.0 - abs(hp % 2.0 - 1.0));
    let m = v - c;

    var rgb: vec3<f32>;
    if hp < 1.0 {
        rgb = vec3f(c, x, 0.0);
    } else if hp < 2.0 {
        rgb = vec3f(x, c, 0.0);
    } else if hp < 3.0 {
        rgb = vec3f(0.0, c, x);
    } else if hp < 4.0 {
        rgb = vec3f(0.0, x, c);
    } else if hp < 5.0 {
        rgb = vec3f(x, 0.0, c);
    } else {
        rgb = vec3f(c, 0.0, x);
    }

    return rgb + vec3f(m, m, m);
}

@fragment
fn fragment(in: UiVertexOutput) -> @location(0) vec4<f32> {
    let uv = in.uv;
    let t = time.x;

    // Unpack parameters
    let border_thickness = params.x;  // In pixels (we'll estimate UV space)
    let glow_radius = params.y;
    let glow_intensity = params.z;
    let chase_speed = params.w;

    let chase_width = chase.x;
    let chase_intensity = chase.y;
    let chase_tail = chase.z;

    // Estimate border thickness in UV space (assuming roughly square-ish aspect)
    // This is approximate - for perfect results we'd need dimensions uniform
    let border_uv = border_thickness / 200.0;  // Assume ~200px typical column width

    // Get perimeter position and edge distance
    let peri = perimeter_position(uv);
    let peri_pos = peri.x;
    let edge_dist = peri.y;

    // Early exit if too far from edge
    if edge_dist > border_uv + glow_radius * 0.3 {
        return vec4f(0.0, 0.0, 0.0, 0.0);
    }

    // Base border intensity - sharp edge with minimal glow
    let in_border = step(edge_dist, border_uv);
    let glow_falloff = exp(-edge_dist / (glow_radius * 0.05)) * glow_intensity * 0.15;
    let base_intensity = in_border * 0.5 + glow_falloff;

    // Chase light position (cycles around perimeter)
    let chase_pos = fract(t * chase_speed);

    // Distance from chase light (with wraparound)
    let chase_dist = perimeter_distance(peri_pos, chase_pos);

    // Chase intensity with tail effect
    // Front: sharp bright spot
    // Tail: gradual fade behind
    let front_intensity = exp(-pow(chase_dist / chase_width, 2.0) * 12.0);

    // Tail direction (chase moves clockwise, so tail is counter-clockwise)
    // Calculate signed distance for tail direction
    var signed_dist = peri_pos - chase_pos;
    if signed_dist > 0.5 { signed_dist -= 1.0; }
    if signed_dist < -0.5 { signed_dist += 1.0; }

    // Tail only appears behind the chase (negative signed_dist means behind)
    let tail_factor = max(0.0, -signed_dist) / chase_tail;
    let tail_intensity = exp(-tail_factor * 3.0) * step(0.0, chase_tail - abs(signed_dist));

    let total_chase = (front_intensity + tail_intensity * 0.5) * chase_intensity;

    // Combine base border + chase
    let total_intensity = base_intensity + total_chase;

    if total_intensity < 0.02 {
        return vec4f(0.0, 0.0, 0.0, 0.0);
    }

    // Color mixing: clean cyan base, rainbow cycling chase
    let base_rgb = color.rgb;

    // Chase color cycles through hues over time
    // chase_color.w controls cycle speed (0 = no cycle, use chase_color; >0 = cycle speed)
    let color_cycle_speed = chase_color.w;
    var chase_rgb: vec3<f32>;
    if color_cycle_speed > 0.01 {
        // Rainbow cycle - hue shifts with time
        let hue = fract(t * color_cycle_speed);
        chase_rgb = hsv_to_rgb(hue, 0.9, 1.0);  // High saturation, full brightness
    } else {
        // Static color from uniform
        chase_rgb = chase_color.rgb;
    }

    // Chase with cycling color
    let chase_bright = front_intensity * 1.8;
    let chase_final = chase_rgb * chase_bright;

    // Breathing animation (subtle pulse on base border)
    let breath = 1.0 + 0.08 * sin(t * 2.5);

    // Base cyan border + rainbow chase overlay
    let final_color = base_rgb * base_intensity * breath + chase_final;
    let alpha = min(total_intensity * color.a, 1.0);

    return vec4f(final_color, alpha);
}
