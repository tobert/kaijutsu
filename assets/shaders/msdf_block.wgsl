// MSDF Per-Block Text Rendering Shader
//
// Renders MSDF glyph quads to per-block textures.
// Supports shader-based hinting, stem darkening, directional AA,
// and gamma correction for high-quality text at any size.

struct Uniforms {
    resolution: vec2<f32>,
    msdf_range: f32,
    time: f32,
    sdf_texel: vec2<f32>,
    hint_amount: f32,
    stem_darkening: f32,
    horz_scale: f32,
    vert_scale: f32,
    text_bias: f32,
    gamma_correction: f32,
}

@group(0) @binding(0) var<uniform> uniforms: Uniforms;
@group(0) @binding(1) var atlas_texture: texture_2d<f32>;
@group(0) @binding(2) var atlas_sampler: sampler;

struct VertexInput {
    @location(0) position: vec2<f32>,  // NDC [-1, 1]
    @location(1) uv: vec2<f32>,        // atlas UV
    @location(2) color: vec4<f32>,     // per-glyph RGBA (unorm)
    @location(3) importance: f32,      // semantic weight
}

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) color: vec4<f32>,
    @location(2) importance: f32,
}

// ============================================================================
// VERTEX SHADER
// ============================================================================

@vertex
fn vertex(in: VertexInput) -> VertexOutput {
    var out: VertexOutput;
    out.clip_position = vec4<f32>(in.position, 0.0, 1.0);
    out.uv = in.uv;
    out.color = in.color;
    out.importance = in.importance;
    return out;
}

// ============================================================================
// MSDF UTILITIES
// ============================================================================

/// Compute the median of three values — core of MSDF distance extraction.
fn median(r: f32, g: f32, b: f32) -> f32 {
    return max(min(r, g), min(max(r, g), b));
}

/// Compute screen-space pixel range for adaptive anti-aliasing.
fn screen_px_range(uv: vec2<f32>) -> f32 {
    let atlas_size = vec2<f32>(textureDimensions(atlas_texture, 0));
    let uv_fwidth = fwidth(uv);
    let pixel_dist = max(uv_fwidth.x, uv_fwidth.y);
    let unit_range = uniforms.msdf_range / atlas_size.x;
    return max(unit_range / pixel_dist, 1.0);
}

/// Shader-based hinting with stem darkening and semantic weighting.
///
/// Detects stroke direction via gradient, applies:
/// - Wider AA for vertical strokes (horz_scale)
/// - Sharper AA for horizontal strokes (vert_scale)
/// - Stem darkening at small sizes (FreeType-style)
/// - Importance-based weight modulation
fn msdf_alpha_hinted(uv: vec2<f32>, bias: f32, importance: f32) -> f32 {
    // Sample center and neighbors
    let sample_c = textureSample(atlas_texture, atlas_sampler, uv);
    let sample_n = textureSample(atlas_texture, atlas_sampler, uv + vec2<f32>(0.0, uniforms.sdf_texel.y));
    let sample_e = textureSample(atlas_texture, atlas_sampler, uv + vec2<f32>(uniforms.sdf_texel.x, 0.0));

    // Get signed distances using MTSDF (median + alpha correction)
    let sd_c = min(median(sample_c.r, sample_c.g, sample_c.b), sample_c.a);
    let sd_n = min(median(sample_n.r, sample_n.g, sample_n.b), sample_n.a);
    let sd_e = min(median(sample_e.r, sample_e.g, sample_e.b), sample_e.a);

    // Compute gradient (perpendicular to stroke edge)
    let sgrad = vec2<f32>(sd_e - sd_c, sd_n - sd_c);
    let sgrad_len = max(length(sgrad), 1.0 / 128.0);
    let grad = sgrad / sgrad_len;

    // vgrad: 0 = vertical stroke edge, 1 = horizontal stroke edge
    let vgrad = abs(grad.y);

    let px_range = screen_px_range(uv);
    let base_doffset = 0.5 / (px_range * 2.0);

    // Stem darkening: thicken thin strokes at small font sizes
    let darkening = uniforms.stem_darkening * clamp(1.0 / px_range, 0.0, 0.5);

    // Semantic weighting: importance modulates stroke thickness
    let weight_adjust = mix(0.02, -0.015, importance);

    let effective_bias = bias - darkening + weight_adjust;

    // Direction-adaptive AA widths
    let hinted_doffset = mix(
        base_doffset * uniforms.horz_scale,
        base_doffset * uniforms.vert_scale,
        vgrad
    );
    let doffset = mix(base_doffset, hinted_doffset, uniforms.hint_amount);

    var alpha = smoothstep(effective_bias - doffset, effective_bias + doffset, sd_c);

    // Darken horizontal strokes slightly for better visual weight
    alpha = pow(alpha, 1.0 + 0.2 * vgrad * uniforms.hint_amount);

    // Gamma correction for light-on-dark compensation
    alpha = pow(alpha, uniforms.gamma_correction);

    return alpha;
}

// ============================================================================
// FRAGMENT SHADER
// ============================================================================

@fragment
fn fragment(in: VertexOutput) -> @location(0) vec4<f32> {
    let text_alpha = msdf_alpha_hinted(in.uv, uniforms.text_bias, in.importance);
    let text_color = in.color.rgb;

    // Premultiplied alpha output
    let final_alpha = text_alpha * in.color.a;
    let premul_rgb = text_color * final_alpha;

    if final_alpha < 0.004 {
        discard;
    }

    return vec4<f32>(premul_rgb, final_alpha);
}
