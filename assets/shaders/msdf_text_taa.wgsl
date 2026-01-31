// MSDF Text TAA Blend Shader
//
// Temporal Anti-Aliasing blend pass for MSDF text rendering.
// Accumulates multiple jittered frames to improve edge quality.
//
// Algorithm:
// 1. Sample current frame (already jittered)
// 2. Sample history at same UV (no reprojection for static text)
// 3. Apply YCoCg variance clipping to prevent ghosting
// 4. Blend based on accumulated confidence
// 5. Output to history texture

struct TaaUniforms {
    // Resolution for UV calculations
    resolution: vec2<f32>,
    // Number of frames accumulated (for blend weight)
    frames_accumulated: u32,
    // Whether TAA is enabled (0 = off, 1 = on)
    taa_enabled: u32,
    // Camera motion delta for reprojection (Phase 4)
    // For now, assume static text (0, 0)
    camera_motion: vec2<f32>,
    // Configurable convergence parameters
    convergence_frames: f32,  // Number of frames to converge (default: 8)
    initial_weight: f32,      // Initial blend weight (default: 0.5)
    final_weight: f32,        // Final blend weight (default: 0.1)
    // Padding for alignment
    _padding: f32,
}

@group(0) @binding(0) var<uniform> uniforms: TaaUniforms;
@group(0) @binding(1) var current_texture: texture_2d<f32>;
@group(0) @binding(2) var history_texture: texture_2d<f32>;
@group(0) @binding(3) var linear_sampler: sampler;

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
}

// ============================================================================
// VERTEX SHADER - Fullscreen triangle
// ============================================================================

@vertex
fn vertex(@builtin(vertex_index) vertex_index: u32) -> VertexOutput {
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
// COLOR SPACE CONVERSION
// ============================================================================

// RGB to YCoCg conversion for variance clipping
// YCoCg provides better correlation for neighborhood clamping
fn rgb_to_ycocg(rgb: vec3<f32>) -> vec3<f32> {
    let y  = 0.25 * rgb.r + 0.5 * rgb.g + 0.25 * rgb.b;
    let co = 0.5 * rgb.r - 0.5 * rgb.b;
    let cg = -0.25 * rgb.r + 0.5 * rgb.g - 0.25 * rgb.b;
    return vec3<f32>(y, co, cg);
}

// YCoCg to RGB conversion
fn ycocg_to_rgb(ycocg: vec3<f32>) -> vec3<f32> {
    let y = ycocg.x;
    let co = ycocg.y;
    let cg = ycocg.z;
    let r = y + co - cg;
    let g = y + cg;
    let b = y - co - cg;
    return vec3<f32>(r, g, b);
}

// ============================================================================
// VARIANCE CLIPPING
// ============================================================================

// Clip history color to AABB defined by current frame neighborhood
// This is the key technique to prevent ghosting on static text
fn clip_aabb(history_ycocg: vec3<f32>, aabb_min: vec3<f32>, aabb_max: vec3<f32>) -> vec3<f32> {
    // Clamp each component to AABB
    return clamp(history_ycocg, aabb_min, aabb_max);
}

// Sample 3x3 neighborhood and compute variance AABB
fn compute_variance_aabb(uv: vec2<f32>, texel_size: vec2<f32>) -> array<vec3<f32>, 2> {
    var samples: array<vec3<f32>, 9>;
    var idx = 0u;

    // Sample 3x3 neighborhood
    for (var dy = -1; dy <= 1; dy = dy + 1) {
        for (var dx = -1; dx <= 1; dx = dx + 1) {
            let offset = vec2<f32>(f32(dx), f32(dy)) * texel_size;
            let rgb = textureSample(current_texture, linear_sampler, uv + offset).rgb;
            samples[idx] = rgb_to_ycocg(rgb);
            idx = idx + 1u;
        }
    }

    // Compute min/max (simple AABB)
    var aabb_min = samples[0];
    var aabb_max = samples[0];
    for (var i = 1u; i < 9u; i = i + 1u) {
        aabb_min = min(aabb_min, samples[i]);
        aabb_max = max(aabb_max, samples[i]);
    }

    // Slightly expand AABB to reduce flickering (from Playdead TAA)
    let aabb_center = (aabb_min + aabb_max) * 0.5;
    let aabb_extent = (aabb_max - aabb_min) * 0.5;
    let expand = 1.25; // 25% expansion
    aabb_min = aabb_center - aabb_extent * expand;
    aabb_max = aabb_center + aabb_extent * expand;

    return array<vec3<f32>, 2>(aabb_min, aabb_max);
}

// ============================================================================
// FRAGMENT SHADER
// ============================================================================

@fragment
fn fragment(in: VertexOutput) -> @location(0) vec4<f32> {
    // Passthrough if TAA disabled
    if uniforms.taa_enabled == 0u {
        return textureSample(current_texture, linear_sampler, in.uv);
    }

    let texel_size = 1.0 / uniforms.resolution;

    // Sample current frame
    let current = textureSample(current_texture, linear_sampler, in.uv);

    // Reprojected UV (no motion for static text, Phase 4 adds camera motion)
    let history_uv = in.uv - uniforms.camera_motion * texel_size;

    // Sample history
    let history = textureSample(history_texture, linear_sampler, history_uv);

    // Check if we have valid history (frames_accumulated > 0)
    if uniforms.frames_accumulated == 0u {
        // First frame - just use current
        return current;
    }

    // Compute variance AABB for clipping
    let aabb = compute_variance_aabb(in.uv, texel_size);
    let aabb_min = aabb[0];
    let aabb_max = aabb[1];

    // Convert history to YCoCg and clip
    let history_ycocg = rgb_to_ycocg(history.rgb);
    let clipped_ycocg = clip_aabb(history_ycocg, aabb_min, aabb_max);
    let clipped_history = ycocg_to_rgb(clipped_ycocg);

    // Compute blend weight based on accumulated frames
    // Start with initial_weight, reduce to final_weight as frames accumulate
    // Uses configurable convergence_frames for tunable fade-in timing
    let accumulation_factor = min(f32(uniforms.frames_accumulated), uniforms.convergence_frames) / uniforms.convergence_frames;
    let blend_weight = mix(uniforms.initial_weight, uniforms.final_weight, accumulation_factor);

    // Blend current and clipped history (both RGB and alpha)
    // Alpha also jitters at edges, so must be accumulated for stable output
    let blended_rgb = mix(clipped_history, current.rgb, blend_weight);
    let blended_alpha = mix(history.a, current.a, blend_weight);

    return vec4<f32>(blended_rgb, blended_alpha);
}
