// Kaijutsu Shader Utilities
// Common functions for UI effects
//
// Attribution:
// - palette() function based on Kishimisu's tutorial: https://www.youtube.com/watch?v=f4s1h2YETNY
// - hash/noise functions from various shadertoy authors (Dave_Hoskins, Inigo Quilez)
// - SDF functions inspired by Inigo Quilez: https://iquilezles.org/articles/distfunctions2d/
// - Ported/adapted via shadplay: https://github.com/alphastrata/shadplay
//
#define_import_path kaijutsu::shaders::common

// ============================================================================
// CONSTANTS
// ============================================================================
const PI: f32 = 3.14159265359;
const TAU: f32 = 6.28318530718;
const HALF_PI: f32 = 1.57079632679;
const PHI: f32 = 1.61803398874;  // Golden ratio
const SQRT2: f32 = 1.41421356237;

// ============================================================================
// COLOR UTILITIES
// ============================================================================

/// HSV to RGB conversion
/// h: 0-1 (hue), s: 0-1 (saturation), v: 0-1 (value)
fn hsv2rgb(c: vec3f) -> vec3f {
    let K = vec4f(1.0, 2.0 / 3.0, 1.0 / 3.0, 3.0);
    let p = abs(fract(c.xxx + K.xyz) * 6.0 - K.www);
    return c.z * mix(K.xxx, clamp(p - K.xxx, vec3f(0.0), vec3f(1.0)), c.y);
}

/// Kishimisu-style color palette - beautiful gradient from a single value
/// Based on: https://www.youtube.com/watch?v=f4s1h2YETNY (Kishimisu)
/// Original technique by Inigo Quilez: https://iquilezles.org/articles/palettes/
/// Tweak a,b,c,d vectors for different color schemes
fn palette(t: f32) -> vec3f {
    // Default: blue-pink cyberpunk
    let a = vec3f(0.5, 0.5, 0.5);
    let b = vec3f(0.5, 0.5, 0.5);
    let c = vec3f(1.0, 1.0, 1.0);
    let d = vec3f(0.263, 0.416, 0.557);  // Shift for blue-cyan-pink
    return a + b * cos(TAU * (c * t + d));
}

/// Cyberpunk palette variant - more pink/cyan
fn palette_cyber(t: f32) -> vec3f {
    let a = vec3f(0.5, 0.5, 0.5);
    let b = vec3f(0.5, 0.5, 0.5);
    let c = vec3f(1.0, 1.0, 0.5);
    let d = vec3f(0.8, 0.9, 0.3);  // Pink-cyan emphasis
    return a + b * cos(TAU * (c * t + d));
}

/// Rainbow palette
fn palette_rainbow(t: f32) -> vec3f {
    return hsv2rgb(vec3f(t, 0.8, 1.0));
}

/// Blend two colors with glow falloff
fn glow_blend(base: vec3f, glow: vec3f, intensity: f32) -> vec3f {
    return base + glow * intensity;
}

// ============================================================================
// NOISE & HASH FUNCTIONS
// ============================================================================

/// Simple 1D hash
fn hash11(p: f32) -> f32 {
    return fract(sin(p * 127.1) * 43758.5453);
}

/// 2D -> 1D hash
fn hash21(p: vec2f) -> f32 {
    return fract(sin(dot(p, vec2f(127.1, 311.7))) * 43758.5453);
}

/// 2D -> 2D hash (good for sparkles)
/// From Dave_Hoskins: https://www.shadertoy.com/view/4djSRW
fn hash22(p: vec2f) -> vec2f {
    var p3 = fract(vec3f(p.xyx) * vec3f(0.1031, 0.1030, 0.0973));
    p3 += dot(p3, p3.yzx + 33.33);
    return fract((p3.xx + p3.yz) * p3.zy);
}

/// Simple value noise
fn noise(p: vec2f) -> f32 {
    let i = floor(p);
    let f = fract(p);
    let u = f * f * (3.0 - 2.0 * f);  // Smoothstep

    return mix(
        mix(hash21(i + vec2f(0.0, 0.0)), hash21(i + vec2f(1.0, 0.0)), u.x),
        mix(hash21(i + vec2f(0.0, 1.0)), hash21(i + vec2f(1.0, 1.0)), u.x),
        u.y
    );
}

/// Fractal brownian motion (layered noise)
fn fbm(p: vec2f, octaves: i32) -> f32 {
    var value = 0.0;
    var amplitude = 0.5;
    var frequency = 1.0;
    var pos = p;

    for (var i = 0; i < octaves; i++) {
        value += amplitude * noise(pos * frequency);
        amplitude *= 0.5;
        frequency *= 2.0;
    }
    return value;
}

// ============================================================================
// TRANSFORM UTILITIES
// ============================================================================

/// 2D rotation matrix (counter-clockwise)
fn rotate2d(theta: f32) -> mat2x2f {
    let c = cos(theta);
    let s = sin(theta);
    return mat2x2f(c, -s, s, c);
}

/// Convert to polar coordinates (angle, radius)
fn to_polar(p: vec2f) -> vec2f {
    return vec2f(atan2(p.y, p.x), length(p));
}

/// Convert from polar to cartesian
fn from_polar(p: vec2f) -> vec2f {
    return vec2f(cos(p.x), sin(p.x)) * p.y;
}

// ============================================================================
// SIGNED DISTANCE FIELDS (SDF)
// ============================================================================

/// Circle SDF
fn sd_circle(p: vec2f, r: f32) -> f32 {
    return length(p) - r;
}

/// Box SDF (centered)
fn sd_box(p: vec2f, b: vec2f) -> f32 {
    let d = abs(p) - b;
    return length(max(d, vec2f(0.0))) + min(max(d.x, d.y), 0.0);
}

/// Rounded box SDF
fn sd_rounded_box(p: vec2f, b: vec2f, r: f32) -> f32 {
    let q = abs(p) - b + r;
    return min(max(q.x, q.y), 0.0) + length(max(q, vec2f(0.0))) - r;
}

/// Ring SDF (circle outline)
fn sd_ring(p: vec2f, r: f32, thickness: f32) -> f32 {
    return abs(length(p) - r) - thickness;
}

/// Border SDF (box outline)
fn sd_border(p: vec2f, b: vec2f, thickness: f32) -> f32 {
    return abs(sd_box(p, b)) - thickness;
}

// ============================================================================
// EFFECT UTILITIES
// ============================================================================

/// Soft glow falloff
fn glow(d: f32, radius: f32, intensity: f32) -> f32 {
    return pow(radius / max(abs(d), 0.0001), intensity);
}

/// Pulse between 0-1 with time
fn pulse(time: f32, speed: f32) -> f32 {
    return 0.5 + 0.5 * sin(time * speed);
}

/// Smooth pulse (eased)
fn pulse_smooth(time: f32, speed: f32) -> f32 {
    let t = 0.5 + 0.5 * sin(time * speed);
    return t * t * (3.0 - 2.0 * t);
}

/// Scanline effect
fn scanlines(uv: vec2f, count: f32, intensity: f32) -> f32 {
    return 1.0 - intensity * (0.5 + 0.5 * sin(uv.y * count * PI));
}

/// Vignette effect (darkens edges)
fn vignette(uv: vec2f, intensity: f32) -> f32 {
    let d = length(uv - 0.5) * 2.0;
    return 1.0 - d * d * intensity;
}
