// Holo Border Shader
// Animated rainbow/gradient border with holographic shimmer
//
// Attribution:
// - Kishimisu palette technique: https://www.youtube.com/watch?v=f4s1h2YETNY
// - IQ palette math: https://iquilezles.org/articles/palettes/
// - Adapted via shadplay: https://github.com/alphastrata/shadplay
//
#import bevy_ui::ui_vertex_output::UiVertexOutput

// Separate bindings to match Bevy's AsBindGroup derive
@group(1) @binding(0) var<uniform> base_color: vec4<f32>; // Base color to blend with
@group(1) @binding(1) var<uniform> params1: vec4<f32>;    // x=saturation, y=speed, z=border_width, w=shimmer_scale
@group(1) @binding(2) var<uniform> params2: vec4<f32>;    // x=rainbow_spread, y=mode, z=unused, w=unused
@group(1) @binding(3) var<uniform> time: vec4<f32>;       // x=elapsed_time

const PI: f32 = 3.14159265359;
const TAU: f32 = 6.28318530718;

// HSV to RGB
fn hsv2rgb(c: vec3f) -> vec3f {
    let K = vec4f(1.0, 2.0 / 3.0, 1.0 / 3.0, 3.0);
    let p = abs(fract(c.xxx + K.xyz) * 6.0 - K.www);
    return c.z * mix(K.xxx, clamp(p - K.xxx, vec3f(0.0), vec3f(1.0)), c.y);
}

// Kishimisu palette
fn palette(t: f32) -> vec3f {
    let a = vec3f(0.5, 0.5, 0.5);
    let b = vec3f(0.5, 0.5, 0.5);
    let c = vec3f(1.0, 1.0, 1.0);
    let d = vec3f(0.263, 0.416, 0.557);
    return a + b * cos(TAU * (c * t + d));
}

// Cyberpunk palette (pink/cyan)
fn palette_cyber(t: f32) -> vec3f {
    let cyan = vec3f(0.34, 0.65, 1.0);
    let pink = vec3f(0.97, 0.47, 0.73);
    return mix(cyan, pink, 0.5 + 0.5 * sin(t * TAU));
}

// Hash for shimmer
fn hash21(p: vec2f) -> f32 {
    return fract(sin(dot(p, vec2f(127.1, 311.7))) * 43758.5453);
}

@fragment
fn fragment(in: UiVertexOutput) -> @location(0) vec4<f32> {
    let uv = in.uv * 2.0 - 1.0;

    // Extract parameters
    let saturation = params1.x;
    let speed = params1.y;
    let border_width = params1.z;
    let shimmer_scale = params1.w;
    let rainbow_spread = params2.x;
    let mode = params2.y;
    let elapsed = time.x;

    // Box SDF for border
    let box_size = vec2f(1.0 - border_width * 2.0);
    let d = abs(uv) - box_size;
    let dist = length(max(d, vec2f(0.0))) + min(max(d.x, d.y), 0.0);

    // Border mask
    let border = smoothstep(border_width, border_width * 0.5, abs(dist));

    if border < 0.01 {
        discard;
    }

    // Position along border for color gradient
    // Use angle around center for smooth gradient
    let angle = atan2(uv.y, uv.x);
    let normalized_angle = (angle + PI) / TAU;  // 0 to 1

    // Add shimmer based on position
    let shimmer_uv = in.uv * shimmer_scale;
    let shimmer = hash21(floor(shimmer_uv) + floor(elapsed * 3.0));
    let shimmer_factor = 0.8 + 0.2 * shimmer;

    // Animated color position
    let color_pos = normalized_angle * rainbow_spread + elapsed * speed;

    // Select palette based on mode
    var holo_color: vec3f;
    if mode < 0.5 {
        // Rainbow mode
        holo_color = hsv2rgb(vec3f(fract(color_pos), saturation, 1.0));
    } else if mode < 1.5 {
        // Cyber mode (pink/cyan)
        holo_color = palette_cyber(color_pos);
    } else {
        // Blend with base color
        holo_color = mix(base_color.rgb, palette(color_pos), saturation);
    }

    // Apply shimmer
    holo_color *= shimmer_factor;

    // Edge glow
    let edge_glow = smoothstep(border_width * 2.0, 0.0, abs(dist)) * 0.3;
    holo_color += edge_glow;

    return vec4f(holo_color, border * base_color.a);
}
