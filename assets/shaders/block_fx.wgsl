// Block FX Shader — post-process layer on Vello-rendered block textures.
//
// Hybrid architecture: Vello draws the structural content (text, fieldset
// borders with label gaps) into a texture. This shader composites that
// texture and adds GPU-native effects: SDF edge glow, animation overlays.
//
// Uniforms:
//   glow_color  - Color for the glow effect (RGBA linear)
//   fx_params   - [glow_radius, glow_intensity, animation_mode, corner_radius]
//     animation_mode: 0=none, 1=breathe, 2=pulse, 3=chase
//
// The Vello texture is bound as a standard 2D texture + sampler.
// Time comes from Bevy's Globals uniform (bind group 0).

#import bevy_ui::ui_vertex_output::UiVertexOutput
#import bevy_render::globals::Globals

@group(0) @binding(1) var<uniform> globals: Globals;

@group(1) @binding(0) var block_texture: texture_2d<f32>;
@group(1) @binding(1) var block_sampler: sampler;
@group(1) @binding(2) var<uniform> glow_color: vec4<f32>;
@group(1) @binding(3) var<uniform> fx_params: vec4<f32>;

// Rounded box SDF (inlined for independence from common.wgsl import)
fn sd_rounded_box(p: vec2<f32>, b: vec2<f32>, r: f32) -> f32 {
    let q = abs(p) - b + r;
    return min(max(q.x, q.y), 0.0) + length(max(q, vec2<f32>(0.0))) - r;
}

@fragment
fn fragment(in: UiVertexOutput) -> @location(0) vec4<f32> {
    // Sample the Vello-rendered texture (text + fieldset borders)
    let tex = textureSample(block_texture, block_sampler, in.uv);


    let glow_radius = fx_params.x;
    let glow_intensity = fx_params.y;
    let anim_mode = fx_params.z;
    let corner_r = fx_params.w;

    // Fast path: no effects — pure texture passthrough
    if glow_radius <= 0.0 {
        return tex;
    }

    // Position in pixel space, centered on the node
    let half_size = in.size * 0.5;
    let p = (in.uv - 0.5) * in.size;

    // SDF distance to the rounded rect edge.
    // d < 0 inside, d = 0 on the edge, d > 0 outside.
    // The Vello fieldset border sits near d ≈ 0, so the SDF
    // aligns with the Vello-drawn border automatically.
    let d = sd_rounded_box(p, half_size, corner_r);

    // Animation modulation
    var anim = 1.0;
    if anim_mode == 1.0 {
        // Breathe: slow sine wave
        anim = 0.7 + 0.3 * sin(globals.time);
    } else if anim_mode == 2.0 {
        // Pulse: faster, sharper
        anim = 0.4 + 0.6 * sin(globals.time * 3.0);
    } else if anim_mode == 3.0 {
        // Chase: position-dependent traveling wave around perimeter
        let angle = atan2(p.y, p.x);
        let phase = angle / 6.28318 + 0.5; // normalize to 0-1
        let wave = fract(phase - globals.time * 0.4);
        // Bright head with soft tail
        anim = smoothstep(0.3, 0.0, wave) + 0.15;
    }

    // Inner edge glow: exponential falloff from the border toward center.
    // exp(d / glow_radius) where d < 0 (inside) gives falloff.
    // Maximum brightness at d = 0 (the border edge).
    let edge_glow = exp(d / glow_radius) * glow_intensity * anim;

    // Composite: glow shows through transparent areas, text/border preserved.
    // The (1.0 - tex.a) factor prevents glow from washing out content.
    let glow = glow_color.rgb * edge_glow * (1.0 - tex.a);
    let glow_alpha = edge_glow * glow_color.a * (1.0 - tex.a);

    return vec4<f32>(
        tex.rgb + glow,
        max(tex.a, glow_alpha),
    );
}
