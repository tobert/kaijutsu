// Ring Guide Shader
// Faint dashed concentric circles at ring boundaries matching the
// radial tree layout algorithm's radii. Updated with camera offset/zoom.
//
// Uniforms:
//   params     - [base_radius, ring_spacing, max_rings, dash_count]
//   time       - [elapsed_time, ...]
//   camera     - [offset_x, offset_y, zoom, line_opacity]
//   dimensions - [width_px, height_px, 0, 0]

#import bevy_ui::ui_vertex_output::UiVertexOutput

@group(1) @binding(0) var<uniform> params: vec4<f32>;
@group(1) @binding(1) var<uniform> time: vec4<f32>;
@group(1) @binding(2) var<uniform> camera: vec4<f32>;
@group(1) @binding(3) var<uniform> dimensions: vec4<f32>;

@fragment
fn fragment(in: UiVertexOutput) -> @location(0) vec4<f32> {
    let uv = in.uv;

    let base_radius = params.x;
    let ring_spacing = params.y;
    let max_rings = i32(params.z);
    let dash_count = params.w;

    let cam_ox = camera.x;
    let cam_oy = camera.y;
    let zoom = camera.z;
    let line_opacity = camera.w;

    let width_px = dimensions.x;
    let height_px = dimensions.y;

    // Convert UV to pixel coordinates relative to constellation center
    let p = uv * vec2<f32>(width_px, height_px)
          - vec2<f32>(width_px * 0.5 + cam_ox, height_px * 0.5 + cam_oy);

    let dist = length(p);
    let angle = atan2(p.y, p.x);

    // Line rendering parameters
    let line_width = 1.0; // pixels
    let dash_gap = 0.45; // fraction of dash cycle that is gap

    var total_alpha = 0.0;

    // Check each ring
    for (var i = 1; i <= 8; i++) {
        if i > max_rings {
            break;
        }

        // Ring radius in screen space
        let ring_r = (base_radius + f32(i) * ring_spacing) * zoom;

        // Distance from this pixel to the ring circle
        let ring_dist = abs(dist - ring_r);

        // Anti-aliased line
        let ring_alpha = 1.0 - smoothstep(line_width * 0.5, line_width * 1.5, ring_dist);

        // Dashed pattern: based on angle around the circle
        let dash_phase = fract(angle * dash_count / 6.28318);
        let dash_alpha = 1.0 - smoothstep(dash_gap - 0.05, dash_gap + 0.05, dash_phase);

        // Fade outer rings more
        let ring_fade = 1.0 - f32(i - 1) * 0.15;

        total_alpha += ring_alpha * dash_alpha * ring_fade;
    }

    total_alpha = min(total_alpha, 1.0) * line_opacity;

    if total_alpha < 0.003 {
        discard;
    }

    // Subtle cool blue-white color
    let ring_color = vec3<f32>(0.4, 0.5, 0.65);

    return vec4<f32>(ring_color * total_alpha, total_alpha);
}
