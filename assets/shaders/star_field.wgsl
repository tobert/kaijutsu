// Star Field Shader
// Procedural star field background for constellation view.
//
// Hash-based star positions with brightness variation, subtle twinkle,
// and color temperature variation. Approximately 80-100 visible stars.
// Slight parallax shift from camera offset for depth illusion.
//
// Uniforms:
//   params     - [density, twinkle_speed, brightness, star_size]
//   time       - [elapsed_time, ...]
//   dimensions - [width_px, height_px, camera_offset_x, camera_offset_y]

#import bevy_ui::ui_vertex_output::UiVertexOutput

@group(1) @binding(0) var<uniform> params: vec4<f32>;
@group(1) @binding(1) var<uniform> time: vec4<f32>;
@group(1) @binding(2) var<uniform> dimensions: vec4<f32>;

// Hash functions (Dave_Hoskins: https://www.shadertoy.com/view/4djSRW)
fn hash21(p: vec2<f32>) -> f32 {
    return fract(sin(dot(p, vec2<f32>(127.1, 311.7))) * 43758.5453);
}

fn hash22(p: vec2<f32>) -> vec2<f32> {
    var p3 = fract(vec3<f32>(p.xyx) * vec3<f32>(0.1031, 0.103, 0.0973));
    p3 += dot(p3, p3.yzx + 33.33);
    return fract((p3.xx + p3.yz) * p3.zy);
}

@fragment
fn fragment(in: UiVertexOutput) -> @location(0) vec4<f32> {
    let uv = in.uv;
    let t = time.x;

    let density = params.x;
    let twinkle_speed = params.y;
    let brightness = params.z;
    let star_size = params.w;

    let width_px = dimensions.x;
    let height_px = dimensions.y;
    let cam_ox = dimensions.z;
    let cam_oy = dimensions.w;

    // Aspect correction so stars aren't stretched
    let aspect = width_px / max(height_px, 1.0);
    var star_uv = uv;
    star_uv.x *= aspect;

    // Subtle parallax: shift UV by camera offset (reduced factor for depth)
    let parallax = 0.0002;
    star_uv += vec2<f32>(cam_ox, cam_oy) * parallax;

    // Grid for star placement
    let grid_uv = star_uv * density;
    let grid_id = floor(grid_uv);
    let grid_fract = fract(grid_uv) - 0.5;

    // Random position jitter within cell
    let rand = hash22(grid_id);
    let offset = (rand - 0.5) * 0.8;

    // Distance from this pixel to the star center
    let star_pos = grid_fract - offset;
    let dist = length(star_pos);

    // Per-star properties via hash
    let star_brightness = hash21(grid_id + 42.0);
    let star_phase = hash21(grid_id + 137.0) * 6.28318;
    let star_twinkle_rate = 0.5 + hash21(grid_id + 271.0) * 1.5;

    // Sparsity: only ~60% of cells have visible stars
    let has_star = step(0.4, star_brightness);

    // Twinkle: slow sinusoidal with per-star phase and rate
    let twinkle = 0.6 + 0.4 * sin(t * twinkle_speed * star_twinkle_rate + star_phase);

    // Star radius varies with brightness
    let size = star_size * (0.3 + 0.7 * star_brightness);

    // Circular dot with soft edge
    let star_alpha = smoothstep(size, size * 0.2, dist) * has_star;

    // Soft glow halo around brighter stars
    let glow = exp(-dist / (size * 4.0)) * 0.3 * step(0.5, star_brightness) * has_star;

    // Color temperature: cool blue-white to warm yellow-white
    let temp = hash21(grid_id + 500.0);
    let cool = vec3<f32>(0.8, 0.85, 1.0);
    let warm = vec3<f32>(1.0, 0.95, 0.85);
    let star_color = mix(cool, warm, temp);

    let final_intensity = (star_alpha + glow) * twinkle * brightness * star_brightness;

    if final_intensity < 0.003 {
        discard;
    }

    return vec4<f32>(star_color * final_intensity, final_intensity);
}
