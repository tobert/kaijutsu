// Shared color and fog helpers for Kaijutsu shaders (docs/newui.md).
//
// Defines the OKLCH rainbow ramp (`rainbow`) matching Rust's `rainbow_oklch`
// in `view::scene_palette` and the linear distance fog helper (`apply_fog`).

const TAU: f32 = 6.28318530718;

/// OKLCH -> OKLab -> linear sRGB rainbow ramp.
/// `t` wraps over [0, 1); equal steps in `t` produce equal perceptual hue steps,
/// and lowering `chroma` fades every hue evenly toward neutral gray.
fn rainbow(t: f32, lightness: f32, chroma: f32) -> vec3<f32> {
    let h = fract(t) * TAU;
    let a = chroma * cos(h);
    let b = chroma * sin(h);

    let l_ = lightness + 0.3963377774 * a + 0.2158037573 * b;
    let m_ = lightness - 0.1055613458 * a - 0.0638541728 * b;
    let s_ = lightness - 0.0894841775 * a - 1.2914855480 * b;

    let l = l_ * l_ * l_;
    let m = m_ * m_ * m_;
    let s = s_ * s_ * s_;

    let r = max(4.0767416621 * l - 3.3077115913 * m + 0.2309699292 * s, 0.0);
    let g = max(-1.2684380046 * l + 2.6097574011 * m - 0.3413193965 * s, 0.0);
    let b_out = max(-0.0041960863 * l - 0.7034186147 * m + 1.7076147010 * s, 0.0);
    return vec3<f32>(r, g, b_out);
}

/// Linear distance fog matching `[heroic.fog]` start/end/color.
fn apply_fog(color: vec3<f32>, fog_color: vec3<f32>, dist: f32, fog_start: f32, fog_end: f32) -> vec3<f32> {
    let f = clamp((dist - fog_start) / max(fog_end - fog_start, 0.001), 0.0, 1.0);
    return mix(color, fog_color, f);
}
