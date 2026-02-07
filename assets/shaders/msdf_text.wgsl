// MSDF Text Rendering Shader
//
// Multi-channel Signed Distance Field text rendering with support for:
// - Smooth anti-aliased edges at any scale
// - Rainbow color cycling effect
// - Glow/outline effects
//
// The MSDF technique stores distance information in RGB channels,
// enabling sharp rendering of text at any zoom level.

struct Uniforms {
    resolution: vec2<f32>,
    msdf_range: f32,
    time: f32,
    rainbow: u32,
    glow_intensity: f32,
    glow_spread: f32,
    // Debug mode: 0=off, 1=dots only, 2=dots+quads
    debug_mode: u32,
    glow_color: vec4<f32>,
    // SDF texel size (1/atlas_width, 1/atlas_height) for gradient sampling
    sdf_texel: vec2<f32>,
    // Hinting strength (0.0 = off, 1.0 = full)
    hint_amount: f32,
    // Stem darkening strength (0.0 = off, ~0.15 = ClearType-like, 0.5 = max)
    // Thickens thin strokes at small font sizes - the #1 ClearType technique
    stem_darkening: f32,
    // TAA jitter offset in pixels (sub-pixel displacement for temporal accumulation)
    // Applied to vertex positions to sample different sub-pixel locations each frame
    jitter_offset: vec2<f32>,
    // Current frame index in the TAA sequence (0-7)
    taa_frame_index: u32,
    // Whether TAA jitter is enabled (0 = off, 1 = on)
    taa_enabled: u32,
    // Horizontal stroke AA scale (1.0-1.3). Wider AA for vertical strokes.
    horz_scale: f32,
    // Vertical stroke AA scale (0.5-0.8). Sharper AA for horizontal strokes.
    vert_scale: f32,
    // SDF threshold for text rendering (0.45-0.55). Default 0.5.
    text_bias: f32,
    // Gamma correction for alpha (< 1.0 widens AA for light-on-dark, > 1.0 for dark-on-light).
    // Default 0.85 compensates for perceptual thinning of light text on dark backgrounds.
    gamma_correction: f32,
}

@group(0) @binding(0) var<uniform> uniforms: Uniforms;
@group(0) @binding(1) var atlas_texture: texture_2d<f32>;
@group(0) @binding(2) var atlas_sampler: sampler;

struct VertexInput {
    @location(0) position: vec3<f32>,  // x, y in NDC, z for depth testing
    @location(1) uv: vec2<f32>,
    @location(2) color: vec4<f32>,
    @location(3) importance: f32,      // semantic weight (0.0 = faded, 0.5 = normal, 1.0 = bold)
}

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) color: vec4<f32>,
    @location(2) screen_pos: vec2<f32>,
    @location(3) importance: f32,
}

// ============================================================================
// VERTEX SHADER
// ============================================================================

@vertex
fn vertex(in: VertexInput) -> VertexOutput {
    var out: VertexOutput;

    // Apply TAA jitter offset for temporal super-resolution
    // The jitter is in pixels, convert to NDC by dividing by resolution and multiplying by 2
    // (NDC range is -1 to 1, so 2 units total per axis)
    var jittered_pos = in.position.xy;
    if uniforms.taa_enabled != 0u {
        // jitter_offset is in range [-0.5, 0.5] pixels
        // Convert to NDC: offset_ndc = offset_px * 2.0 / resolution
        let jitter_ndc = uniforms.jitter_offset * 2.0 / uniforms.resolution;
        jittered_pos = jittered_pos + jitter_ndc;
    }

    // Use z from input for depth testing (earlier glyphs have lower z, winning depth test)
    out.clip_position = vec4<f32>(jittered_pos, in.position.z, 1.0);
    out.uv = in.uv;
    out.color = in.color;
    // Screen pos should use original position, not jittered (for effects like rainbow)
    out.screen_pos = (in.position.xy + 1.0) * 0.5 * uniforms.resolution;
    out.importance = in.importance;
    return out;
}

// ============================================================================
// MSDF UTILITIES
// ============================================================================

/// Compute the median of three values.
/// This is the core of MSDF - the signed distance is encoded across RGB channels.
fn median(r: f32, g: f32, b: f32) -> f32 {
    return max(min(r, g), min(max(r, g), b));
}

/// Compute screen-space pixel range for adaptive anti-aliasing.
/// This ensures consistent edge sharpness regardless of zoom level.
fn screen_px_range(uv: vec2<f32>) -> f32 {
    let atlas_size = vec2<f32>(textureDimensions(atlas_texture, 0));
    // Calculate how much UV changes per screen pixel
    let uv_fwidth = fwidth(uv);
    // Use the max change to be conservative (avoid aliasing)
    let pixel_dist = max(uv_fwidth.x, uv_fwidth.y);

    // The range of the distance field in UV units
    // msdf_range is in pixels, so we divide by atlas size
    let unit_range = uniforms.msdf_range / atlas_size.x; // Assumes square pixels

    // Calculate range in screen pixels
    // Value = (range_in_uv) / (uv_per_pixel)
    return max(unit_range / pixel_dist, 1.0);
}

/// Sample MTSDF and compute alpha at a given bias.
/// bias = 0.5 for normal text, lower for glow/outline.
///
/// MTSDF stores the multi-channel SDF in RGB and the true SDF in alpha.
/// Using min(msdf, true_sdf) corrects corner artifacts where MSDF median fails.
fn msdf_alpha_at(uv: vec2<f32>, bias: f32) -> f32 {
    let sample = textureSample(atlas_texture, atlas_sampler, uv);
    // MSDF median from RGB channels
    let msdf_sd = median(sample.r, sample.g, sample.b);
    // MTSDF: use alpha channel (true SDF) to correct corners
    let sd = min(msdf_sd, sample.a);

    let px_range = screen_px_range(uv);
    // Convert distance to screen pixels
    // Multiplying by 2.0 provides good balance between sharpness and smoothness
    let dist = (sd - bias) * px_range * 2.0;
    return clamp(dist + 0.5, 0.0, 1.0);
}

/// Shader-based hinting using gradient detection, with stem darkening and semantic weighting.
///
/// This technique from webgl_fonts (astiopin) improves small text quality by:
/// 1. Sampling neighboring texels to detect stroke direction
/// 2. Applying wider AA for vertical strokes (which appear thinner)
/// 3. Applying sharper AA for horizontal strokes (stems, crossbars)
/// 4. Optionally darkening horizontal strokes for better weight
///
/// Stem darkening (FreeType-style) thickens thin strokes at small sizes:
/// - Shifts the SDF threshold inward proportional to 1/font_size
/// - Makes 'i', 'l', 't' strokes match ClearType weight at 12-16px
///
/// Semantic weighting (importance) adjusts stroke weight based on context:
/// - 0.0 = thin/faded (inactive, distant from cursor)
/// - 0.5 = normal weight (default)
/// - 1.0 = bold/emphasized (cursor proximity, agent activity, selection)
///
/// The result mimics TrueType hinting's focus on horizontal alignment,
/// making stems and crossbars crisper at small sizes.
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
    // sgrad = (change in east direction, change in north direction)
    let sgrad = vec2<f32>(sd_e - sd_c, sd_n - sd_c);
    let sgrad_len = max(length(sgrad), 1.0 / 128.0); // Prevent division by zero
    let grad = sgrad / sgrad_len;

    // vgrad: 0.0 = vertical stroke edge (horizontal gradient)
    //        1.0 = horizontal stroke edge (vertical gradient)
    // Vertical strokes (like 'l', 'I') have horizontal gradients (grad.y ≈ 0)
    // Horizontal strokes (crossbar of 'T', 'H') have vertical gradients (grad.y ≈ 1)
    let vgrad = abs(grad.y);

    // Compute base distance offset for antialiasing
    let px_range = screen_px_range(uv);
    let base_doffset = 0.5 / (px_range * 2.0);

    // === STEM DARKENING ===
    // At small font sizes, thin strokes appear too light. FreeType compensates
    // by shifting the SDF threshold inward (lowering bias), making strokes wider.
    // The darkening is inversely proportional to px_range (which correlates to font size).
    // clamp(1.0 / px_range, 0.0, 0.5) gives ~0.5 at 2px range, ~0.1 at 10px range
    let darkening = uniforms.stem_darkening * clamp(1.0 / px_range, 0.0, 0.5);

    // === SEMANTIC WEIGHTING ===
    // Adjust bias based on importance: lower bias = thicker strokes
    // importance 0.0 → +0.02 (thinner/faded)
    // importance 0.5 → 0.00 (normal)
    // importance 1.0 → -0.015 (bolder)
    let weight_adjust = mix(0.02, -0.015, importance);

    // Combine darkening and weight adjustment
    let effective_bias = bias - darkening + weight_adjust;

    // Apply different AA widths based on stroke direction
    // - Vertical strokes (vgrad ≈ 0): wider AA (horz_scale, default 1.1)
    // - Horizontal strokes (vgrad ≈ 1): sharper AA (vert_scale, default 0.6)
    // These values come from theme.rhai for hot-reload tuning
    let hinted_doffset = mix(base_doffset * uniforms.horz_scale, base_doffset * uniforms.vert_scale, vgrad);

    // Interpolate between unhinted and hinted based on hint_amount
    let doffset = mix(base_doffset, hinted_doffset, uniforms.hint_amount);

    // Compute alpha with smoothstep for antialiasing
    // Use effective_bias (with stem darkening + importance applied) instead of raw bias
    var alpha = smoothstep(effective_bias - doffset, effective_bias + doffset, sd_c);

    // Optionally darken horizontal strokes slightly for better visual weight
    // This compensates for the sharper AA making them appear lighter
    alpha = pow(alpha, 1.0 + 0.2 * vgrad * uniforms.hint_amount);

    // Gamma-correct alpha: compensates for perceptual non-linearity.
    // Values < 1.0 widen the AA transition (thicker light-on-dark text).
    // Values > 1.0 narrow it (thicker dark-on-light text).
    alpha = pow(alpha, uniforms.gamma_correction);

    return alpha;
}

// ============================================================================
// COLOR EFFECTS
// ============================================================================

/// HSV to RGB conversion for rainbow effect.
fn hsv_to_rgb(h: f32, s: f32, v: f32) -> vec3<f32> {
    let c = v * s;
    let x = c * (1.0 - abs(fract(h * 6.0) * 2.0 - 1.0));
    let m = v - c;

    var rgb: vec3<f32>;
    let h6 = h * 6.0;
    if h6 < 1.0 {
        rgb = vec3<f32>(c, x, 0.0);
    } else if h6 < 2.0 {
        rgb = vec3<f32>(x, c, 0.0);
    } else if h6 < 3.0 {
        rgb = vec3<f32>(0.0, c, x);
    } else if h6 < 4.0 {
        rgb = vec3<f32>(0.0, x, c);
    } else if h6 < 5.0 {
        rgb = vec3<f32>(x, 0.0, c);
    } else {
        rgb = vec3<f32>(c, 0.0, x);
    }

    return rgb + m;
}

/// Generate rainbow color based on position and time.
fn rainbow_color(screen_x: f32, time: f32) -> vec3<f32> {
    let hue = fract(screen_x * 0.002 + time * 0.3);
    return hsv_to_rgb(hue, 0.8, 1.0);
}

/// Blend a layer on top of base using premultiplied alpha.
/// Both base and the result are in premultiplied form (rgb already multiplied by alpha).
/// This prevents double-blending artifacts when adjacent glyph quads overlap.
fn blend_over_premultiplied(base: vec4<f32>, layer_color: vec3<f32>, layer_alpha: f32) -> vec4<f32> {
    // Premultiplied color for the layer
    let premul_layer = layer_color * layer_alpha;
    // Porter-Duff "over" with premultiplied alpha: dst' = src + dst * (1 - src_alpha)
    let blended_rgb = premul_layer + base.rgb * (1.0 - layer_alpha);
    let blended_alpha = layer_alpha + base.a * (1.0 - layer_alpha);
    return vec4<f32>(blended_rgb, blended_alpha);
}

// ============================================================================
// FRAGMENT SHADER
// ============================================================================

// ============================================================================
// DEBUG HELPERS
// ============================================================================

/// Check if this is a debug primitive (marked with special color encoding).
/// Debug primitives have color.a < 0.01 and use color.rgb as solid color.
fn is_debug_primitive(color: vec4<f32>) -> bool {
    // Debug primitives use a near-zero alpha as a marker
    return color.a < 0.01;
}

// ============================================================================
// FRAGMENT SHADER
// ============================================================================

@fragment
fn fragment(in: VertexOutput) -> @location(0) vec4<f32> {
    // === DEBUG PRIMITIVE RENDERING ===
    // Debug primitives are marked with alpha < 0.01 and rendered as solid color
    if is_debug_primitive(in.color) {
        // Render debug geometry as solid color (the color encodes what type)
        return vec4<f32>(in.color.rgb, 1.0);
    }

    // === SHADER DEBUG MODES ===
    // debug_mode 3: Show raw median distance (grayscale)
    // debug_mode 4: Show computed alpha (grayscale)
    // debug_mode 5: Show hard threshold (binary black/white at sd=0.5)
    if uniforms.debug_mode >= 3u {
        let sample = textureSample(atlas_texture, atlas_sampler, in.uv);
        let sd_msdf = median(sample.r, sample.g, sample.b);
        let sd = min(sd_msdf, sample.a);

        if uniforms.debug_mode == 3u {
            // Raw median distance
            return vec4<f32>(sd, sd, sd, 1.0);
        } else if uniforms.debug_mode == 4u {
            // Computed alpha (uses text_bias from theme)
            let alpha = msdf_alpha_at(in.uv, uniforms.text_bias);
            return vec4<f32>(alpha, alpha, alpha, 1.0);
        } else if uniforms.debug_mode == 5u {
            // Hard threshold (uses text_bias from theme)
            if sd >= uniforms.text_bias {
                return vec4<f32>(1.0, 1.0, 1.0, 1.0);
            } else {
                return vec4<f32>(0.0, 0.0, 0.0, 1.0);
            }
        }
    }

    var output = vec4<f32>(0.0);

    // === GLOW LAYER ===
    // Render glow first (behind text) if enabled
    if uniforms.glow_intensity > 0.0 {
        // Sample at expanded distance for glow (relative to text_bias)
        let glow_bias = uniforms.text_bias - uniforms.glow_spread * 0.05;
        let glow_alpha = msdf_alpha_at(in.uv, glow_bias) * uniforms.glow_intensity;
        output = blend_over_premultiplied(output, uniforms.glow_color.rgb, glow_alpha * uniforms.glow_color.a);
    }

    // === MAIN TEXT ===
    // Use hinted alpha for main text to improve small text quality
    // The hinting varies AA width based on stroke direction:
    // - Horizontal strokes (stems, crossbars) get sharper edges
    // - Vertical strokes get wider AA to maintain smooth appearance
    // Importance modulates stroke weight: 0.0 = thin/faded, 0.5 = normal, 1.0 = bold
    // text_bias from theme controls overall stroke thickness (default 0.5)
    let text_alpha = msdf_alpha_hinted(in.uv, uniforms.text_bias, in.importance);

    // Determine text color
    var text_color = in.color.rgb;
    if uniforms.rainbow != 0u {
        text_color = rainbow_color(in.screen_pos.x, uniforms.time);
    }

    // Blend text using premultiplied alpha
    output = blend_over_premultiplied(output, text_color, text_alpha * in.color.a);

    // === LATE DISCARD ===
    // Discard pixels with no visible ink to prevent them writing to the depth buffer.
    // Previous approach discarded based on raw SDF distance (sd < 0.48), but this caused
    // transparent padding pixels to pass the threshold and write to depth, occluding the
    // visible edges of adjacent glyphs (the "smeared letters" bug). By discarding based
    // on final alpha instead, only pixels with actual ink claim depth buffer territory.
    if output.a < 0.01 {
        discard;
    }

    // Output is already in premultiplied form (rgb * alpha) which matches our blend state
    return output;
}