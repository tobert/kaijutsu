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
}

@group(0) @binding(0) var<uniform> uniforms: Uniforms;
@group(0) @binding(1) var atlas_texture: texture_2d<f32>;
@group(0) @binding(2) var atlas_sampler: sampler;

struct VertexInput {
    @location(0) position: vec2<f32>,
    @location(1) uv: vec2<f32>,
    @location(2) color: vec4<f32>,
}

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) color: vec4<f32>,
    @location(2) screen_pos: vec2<f32>,
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
    out.screen_pos = (in.position + 1.0) * 0.5 * uniforms.resolution;
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
    // Convert distance to screen pixels with steeper falloff
    // Multiplying by 2.0 makes the transition sharper, reducing faint edge artifacts
    let dist = (sd - bias) * px_range * 2.0;
    return clamp(dist + 0.5, 0.0, 1.0);
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

/// Blend a layer on top of base using alpha.
fn blend_over(base: vec4<f32>, layer: vec3<f32>, layer_alpha: f32) -> vec4<f32> {
    let blended = layer * layer_alpha + base.rgb * (1.0 - layer_alpha);
    let alpha = layer_alpha + base.a * (1.0 - layer_alpha);
    return vec4<f32>(blended, alpha);
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
            // Computed alpha
            let alpha = msdf_alpha_at(in.uv, 0.5);
            return vec4<f32>(alpha, alpha, alpha, 1.0);
        } else if uniforms.debug_mode == 5u {
            // Hard threshold
            if sd >= 0.5 {
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
        // Sample at expanded distance for glow
        let glow_bias = 0.5 - uniforms.glow_spread * 0.05;
        let glow_alpha = msdf_alpha_at(in.uv, glow_bias) * uniforms.glow_intensity;
        output = blend_over(output, uniforms.glow_color.rgb, glow_alpha * uniforms.glow_color.a);
    }

    // === MAIN TEXT ===
    // Sample and get signed distance for early discard
    let sample = textureSample(atlas_texture, atlas_sampler, in.uv);
    let msdf_sd = median(sample.r, sample.g, sample.b);
    let sd = min(msdf_sd, sample.a);

    // Early discard for pixels clearly outside the glyph.
    // Using a conservative threshold to preserve anti-aliasing at small sizes.
    if sd < 0.2 {
        discard;
    }

    let text_alpha = msdf_alpha_at(in.uv, 0.5);

    // Determine text color
    var text_color = in.color.rgb;
    if uniforms.rainbow != 0u {
        text_color = rainbow_color(in.screen_pos.x, uniforms.time);
    }

    // Blend text
    output = blend_over(output, text_color, text_alpha * in.color.a);

    return output;
}