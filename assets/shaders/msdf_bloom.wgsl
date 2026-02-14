// MSDF Text Bloom Shader
//
// Post-process bloom for MSDF text glow effect.
// Three entry points in one shader file:
// 1. Extract: read intermediate texture, output alpha * glow_color
// 2. Blur: 9-tap separable Gaussian (direction from uniform)
// 3. Composite: output blurred glow for "behind" blending onto intermediate

struct BloomUniforms {
    // Glow color (RGB) and alpha — vec4 first for 16-byte alignment
    glow_color: vec4<f32>,
    // Blur direction: (1,0) for horizontal, (0,1) for vertical
    blur_direction: vec2<f32>,
    // Texel size (1/width, 1/height) of the texture being sampled
    texel_size: vec2<f32>,
    // Glow intensity (0.0-1.0)
    glow_intensity: f32,
    // Padding to reach 48 bytes (struct align = 16)
    _padding1: f32,
    _padding2: f32,
    _padding3: f32,
}

@group(0) @binding(0) var<uniform> uniforms: BloomUniforms;
@group(0) @binding(1) var input_texture: texture_2d<f32>;
@group(0) @binding(2) var input_sampler: sampler;

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
}

// ============================================================================
// VERTEX SHADER - Fullscreen triangle (shared by all passes)
// ============================================================================

@vertex
fn fullscreen_vertex(@builtin(vertex_index) vertex_index: u32) -> VertexOutput {
    // Generate fullscreen triangle vertices
    // vertex 0: (-1, -1), vertex 1: (3, -1), vertex 2: (-1, 3)
    var out: VertexOutput;
    let x = f32(i32(vertex_index & 1u) * 4 - 1);
    let y = f32(i32(vertex_index >> 1u) * 4 - 1);
    out.position = vec4<f32>(x, y, 0.0, 1.0);
    // UV: (0, 1) at top-left, (1, 0) at bottom-right
    out.uv = vec2<f32>((x + 1.0) * 0.5, (1.0 - y) * 0.5);
    return out;
}

// ============================================================================
// EXTRACT PASS - Read intermediate alpha, output tinted glow source
// ============================================================================

@fragment
fn extract(in: VertexOutput) -> @location(0) vec4<f32> {
    let sample = textureSample(input_texture, input_sampler, in.uv);
    // Use the text alpha to create a glow source tinted with glow_color
    let glow_rgb = uniforms.glow_color.rgb * sample.a * uniforms.glow_intensity;
    let glow_a = sample.a * uniforms.glow_intensity * uniforms.glow_color.a;
    return vec4<f32>(glow_rgb, glow_a);
}

// ============================================================================
// BLUR PASS - 9-tap separable Gaussian, direction from uniform
// ============================================================================

@fragment
fn blur(in: VertexOutput) -> @location(0) vec4<f32> {
    // 9-tap Gaussian kernel, sigma ~= 2 texels
    // Weights: [0.0162, 0.0540, 0.1216, 0.1836, 0.2492, 0.1836, 0.1216, 0.0540, 0.0162]
    // (normalized to sum to 1.0)
    let w0 = 0.2492;  // center
    let w1 = 0.1836;  // +/- 1
    let w2 = 0.1216;  // +/- 2
    let w3 = 0.0540;  // +/- 3
    let w4 = 0.0162;  // +/- 4

    let step = uniforms.blur_direction * uniforms.texel_size;

    var color = textureSample(input_texture, input_sampler, in.uv) * w0;
    color += textureSample(input_texture, input_sampler, in.uv + step * 1.0) * w1;
    color += textureSample(input_texture, input_sampler, in.uv - step * 1.0) * w1;
    color += textureSample(input_texture, input_sampler, in.uv + step * 2.0) * w2;
    color += textureSample(input_texture, input_sampler, in.uv - step * 2.0) * w2;
    color += textureSample(input_texture, input_sampler, in.uv + step * 3.0) * w3;
    color += textureSample(input_texture, input_sampler, in.uv - step * 3.0) * w3;
    color += textureSample(input_texture, input_sampler, in.uv + step * 4.0) * w4;
    color += textureSample(input_texture, input_sampler, in.uv - step * 4.0) * w4;

    return color;
}

// ============================================================================
// COMPOSITE PASS - Output blurred glow for "behind" blending
// ============================================================================

@fragment
fn composite(in: VertexOutput) -> @location(0) vec4<f32> {
    // Simply sample the blurred glow texture.
    // The render pass uses "behind" blend state (OneMinusDstAlpha, One)
    // so glow only appears where the intermediate texture is transparent.
    return textureSample(input_texture, input_sampler, in.uv);
}
