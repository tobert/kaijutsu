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
    _padding: f32,
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
    let unit_range = vec2<f32>(uniforms.msdf_range) / vec2<f32>(textureDimensions(atlas_texture, 0));
    let screen_tex_size = vec2<f32>(1.0) / fwidth(uv);
    return max(0.5 * dot(unit_range, screen_tex_size), 1.0);
}

/// Sample MSDF and compute alpha at a given bias.
/// bias = 0.5 for normal text, lower for glow/outline.
fn msdf_alpha_at(uv: vec2<f32>, bias: f32) -> f32 {
    let sample = textureSample(atlas_texture, atlas_sampler, uv);
    let sd = median(sample.r, sample.g, sample.b);
    let px_range = screen_px_range(uv);
    let dist = px_range * (sd - bias);
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

@fragment
fn fragment(in: VertexOutput) -> @location(0) vec4<f32> {
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
    let text_alpha = msdf_alpha_at(in.uv, 0.5);

    // Determine text color
    var text_color = in.color.rgb;
    if uniforms.rainbow != 0u {
        text_color = rainbow_color(in.screen_pos.x, uniforms.time);
    }

    // Blend text on top
    output = blend_over(output, text_color, text_alpha * in.color.a);

    // Discard fully transparent pixels for performance
    if output.a < 0.001 {
        discard;
    }

    return output;
}
