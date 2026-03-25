// Block FX Shader — post-process layer on MSDF-rendered block textures.
//
// MSDF renders text to per-block textures. This shader composites the
// texture and adds GPU-native effects: SDF edge glow, animation overlays,
// text halo, and cursor beam.
//
// Uniforms:
//   glow_color       - Color for the border glow effect (RGBA linear)
//   fx_params        - [glow_radius, glow_intensity, animation_mode, corner_radius]
//     animation_mode: 0=none, 1=breathe, 2=pulse, 3=chase
//   text_glow_color  - Color for text halo (RGBA linear)
//   text_glow_params - [radius_px, 0, 0, 0]  (radius=0 disables)
//   cursor_params    - [x_uv, y_uv, width_uv, height_uv] (all 0 = disabled)
//   cursor_color     - RGBA color for cursor beam (linear)

#import bevy_ui::ui_vertex_output::UiVertexOutput
#import bevy_render::globals::Globals

@group(0) @binding(1) var<uniform> globals: Globals;

@group(1) @binding(0) var block_texture: texture_2d<f32>;
@group(1) @binding(1) var block_sampler: sampler;
@group(1) @binding(2) var<uniform> glow_color: vec4<f32>;
@group(1) @binding(3) var<uniform> fx_params: vec4<f32>;
@group(1) @binding(4) var<uniform> text_glow_color: vec4<f32>;
@group(1) @binding(5) var<uniform> text_glow_params: vec4<f32>;
@group(1) @binding(6) var<uniform> cursor_params: vec4<f32>;
@group(1) @binding(7) var<uniform> cursor_color: vec4<f32>;

// Rounded box SDF (inlined for independence from common.wgsl import)
fn sd_rounded_box(p: vec2<f32>, b: vec2<f32>, r: f32) -> f32 {
    let q = abs(p) - b + r;
    return min(max(q.x, q.y), 0.0) + length(max(q, vec2<f32>(0.0))) - r;
}

// 9-tap text glow: samples 8 neighbors around each pixel to detect nearby
// text alpha, then blends a colored halo behind transparent areas.
fn text_glow_alpha(uv: vec2<f32>, radius_px: f32) -> f32 {
    let dims = vec2<f32>(textureDimensions(block_texture, 0));
    let step = radius_px / dims;

    let acc =
        textureSampleLevel(block_texture, block_sampler, uv + vec2<f32>( 0.0, -step.y), 0.0).a * 2.0 +
        textureSampleLevel(block_texture, block_sampler, uv + vec2<f32>( 0.0,  step.y), 0.0).a * 2.0 +
        textureSampleLevel(block_texture, block_sampler, uv + vec2<f32>(-step.x,  0.0), 0.0).a * 2.0 +
        textureSampleLevel(block_texture, block_sampler, uv + vec2<f32>( step.x,  0.0), 0.0).a * 2.0 +
        textureSampleLevel(block_texture, block_sampler, uv + vec2<f32>(-step.x, -step.y), 0.0).a +
        textureSampleLevel(block_texture, block_sampler, uv + vec2<f32>( step.x, -step.y), 0.0).a +
        textureSampleLevel(block_texture, block_sampler, uv + vec2<f32>(-step.x,  step.y), 0.0).a +
        textureSampleLevel(block_texture, block_sampler, uv + vec2<f32>( step.x,  step.y), 0.0).a;

    return acc / 12.0;
}

@fragment
fn fragment(in: UiVertexOutput) -> @location(0) vec4<f32> {
    let tex = textureSample(block_texture, block_sampler, in.uv);

    let glow_radius = fx_params.x;
    let glow_intensity = fx_params.y;
    let anim_mode = fx_params.z;
    let corner_r = fx_params.w;
    let tg_radius = text_glow_params.x;
    let has_cursor = cursor_params.z > 0.0;

    // Fast path: no effects — pure texture passthrough
    if glow_radius <= 0.0 && tg_radius <= 0.0 && !has_cursor {
        return tex;
    }

    // --- Text glow (composited first, behind everything) ---
    var result = tex;
    if tg_radius > 0.0 {
        let glow_mask = text_glow_alpha(in.uv, tg_radius);
        let intensity = glow_mask * text_glow_color.a;
        let behind = 1.0 - tex.a;
        result = vec4<f32>(
            tex.rgb + text_glow_color.rgb * intensity * behind,
            tex.a + intensity * behind,
        );
    }

    // --- Cursor beam (sharp rect in UV space, composited over text) ---
    if has_cursor {
        let cx = cursor_params.x;
        let cy = cursor_params.y;
        let cw = cursor_params.z;
        let ch = cursor_params.w;

        if in.uv.x >= cx && in.uv.x <= cx + cw && in.uv.y >= cy && in.uv.y <= cy + ch {
            // Over composite: cursor on top of text
            let ca = cursor_color.a;
            result = vec4<f32>(
                result.rgb * (1.0 - ca) + cursor_color.rgb * ca,
                result.a * (1.0 - ca) + ca,
            );
        }
    }

    // --- Border glow (SDF-based, composited on top) ---
    if glow_radius > 0.0 {
        let half_size = in.size * 0.5;
        let p = (in.uv - 0.5) * in.size;

        let d = sd_rounded_box(p, half_size, corner_r);

        var anim = 1.0;
        if anim_mode == 1.0 {
            anim = 0.7 + 0.3 * sin(globals.time);
        } else if anim_mode == 2.0 {
            anim = 0.4 + 0.6 * sin(globals.time * 3.0);
        } else if anim_mode == 3.0 {
            let angle = atan2(p.y, p.x);
            let phase = angle / 6.28318 + 0.5;
            let wave = fract(phase - globals.time * 0.4);
            anim = smoothstep(0.3, 0.0, wave) + 0.15;
        }

        let edge_glow = exp(d / glow_radius) * glow_intensity * anim;

        let border_glow = glow_color.rgb * edge_glow * (1.0 - result.a);
        let border_alpha = edge_glow * glow_color.a * (1.0 - result.a);

        result = vec4<f32>(
            result.rgb + border_glow,
            max(result.a, border_alpha),
        );
    }

    return result;
}
