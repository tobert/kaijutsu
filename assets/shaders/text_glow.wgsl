// Text Glow Shader
// Theme-reactive effects via ShaderEffectContext
//
// Two modes: standard glow (backing) or icy sheen (reflective plane)
// params.w > 0.5 triggers icy mode
//
// Uniforms:
//   color      - Base glow color (RGBA)
//   params     - [radius, intensity, falloff, mode (0=glow, >0.5=icy)]
//   time       - [elapsed_time, ...]
//   effect_glow - Theme context: [glow_radius, glow_intensity, glow_falloff, sheen_speed]
//   effect_anim - Theme context: [sparkle_threshold, breathe_speed, breathe_amplitude, _]
//   theme_accent - Theme accent color (linear space)

#import bevy_ui::ui_vertex_output::UiVertexOutput

@group(1) @binding(0) var<uniform> color: vec4<f32>;
@group(1) @binding(1) var<uniform> params: vec4<f32>;
@group(1) @binding(2) var<uniform> time: vec4<f32>;
@group(1) @binding(3) var<uniform> effect_glow: vec4<f32>;   // from ShaderEffectContext
@group(1) @binding(4) var<uniform> effect_anim: vec4<f32>;   // from ShaderEffectContext
@group(1) @binding(5) var<uniform> theme_accent: vec4<f32>;  // from ShaderEffectContext

// Simple hash for sparkle effect
fn hash(p: vec2<f32>) -> f32 {
    let k = vec2<f32>(0.3183099, 0.3678794);
    let q = p * k + k.yx;
    return fract(16.0 * k.x * fract(q.x * q.y * (q.x + q.y)));
}

@fragment
fn fragment(in: UiVertexOutput) -> @location(0) vec4<f32> {
    let uv = in.uv;
    let t = time.x;
    let icy_mode = params.w > 0.5;

    // Unpack theme effect parameters
    let ctx_glow_radius = effect_glow.x;
    let ctx_glow_intensity = effect_glow.y;
    let ctx_glow_falloff = effect_glow.z;
    let ctx_sheen_speed = effect_glow.w;
    let ctx_sparkle_threshold = effect_anim.x;
    let ctx_breathe_speed = effect_anim.y;
    let ctx_breathe_amplitude = effect_anim.z;

    if icy_mode {
        // === ICY SHEEN MODE ===
        // Horizontal reflective plane effect with theme-reactive parameters
        let intensity = params.y;

        // Horizontal gradient - brighter in center, fade at edges
        let h_fade = 1.0 - pow(abs(uv.x - 0.5) * 2.0, 2.0);

        // Vertical gradient - brighter at top (y=0, closer to text above), fade down
        let v_fade = 1.0 - uv.y * 0.5;

        // Traveling highlight (speed from theme)
        let highlight_pos = fract(t * ctx_sheen_speed);
        let highlight_dist = abs(uv.x - highlight_pos);
        let highlight = exp(-highlight_dist * highlight_dist * 30.0) * 0.4;

        // Sparkle/crystalline effect (threshold from theme)
        let sparkle_uv = floor(uv * 40.0);
        let sparkle_rand = hash(sparkle_uv);
        let sparkle_phase = sparkle_rand * 6.28 + t * 3.0;
        let sparkle = max(0.0, sin(sparkle_phase)) * step(ctx_sparkle_threshold, sparkle_rand) * 0.5;

        // Subtle shimmer waves
        let shimmer = 0.08 * sin(uv.x * 20.0 - t * 2.0) * sin(uv.y * 10.0 + t);

        let total = (h_fade * v_fade + highlight + sparkle + shimmer) * intensity;

        // Blend between material color and theme accent for sparkles
        let base_rgb = color.rgb;
        let accent_blend = mix(base_rgb, theme_accent.rgb, 0.3);
        let final_color = accent_blend * total;

        // Add slight white highlight to sparkles
        let sparkle_white = vec3<f32>(1.0, 1.0, 1.0) * sparkle * 0.3;

        return vec4f(final_color + sparkle_white, total * color.a);
    } else {
        // === STANDARD GLOW MODE ===
        // Use theme effect parameters for glow
        let centered = uv - 0.5;

        // Blend material params with theme context (material can override)
        // If material params are non-zero, use them; otherwise fall back to theme
        let radius = select(ctx_glow_radius, params.x, params.x > 0.01);
        let intensity = select(ctx_glow_intensity, params.y, params.y > 0.01);
        let falloff = select(ctx_glow_falloff, params.z, params.z > 0.01);
        let edge_boost = 0.3; // Fixed for now

        // Breathing animation (speed + amplitude from theme)
        let breath = 1.0 + ctx_breathe_amplitude * sin(t * ctx_breathe_speed);

        // Soft center glow (radius, falloff from theme)
        let dist_center = length(centered);
        let glow = exp(-pow(dist_center / radius, falloff)) * intensity * breath;

        // Diagonal shimmer (speed from theme)
        let wave_pos = (uv.x + uv.y) * 3.0 - t * ctx_sheen_speed * 3.0;
        let shimmer = 0.05 * sin(wave_pos * 6.28) * intensity;

        // Edge enhancement
        let dist_edge_x = min(uv.x, 1.0 - uv.x);
        let dist_edge_y = min(uv.y, 1.0 - uv.y);
        let dist_edge = min(dist_edge_x, dist_edge_y);
        let edge_glow = smoothstep(0.0, 0.15, dist_edge) * edge_boost * 0.5;

        // Top-light gradient
        let top_light = (1.0 - uv.y) * 0.1 * intensity;

        let total_glow = glow + edge_glow + top_light + shimmer;

        // Blend material color with theme accent subtly
        let blended_color = mix(color.rgb, theme_accent.rgb, 0.15);
        let final_color = blended_color * total_glow;
        let alpha = min(total_glow * color.a, color.a);

        return vec4f(final_color, alpha);
    }
}
