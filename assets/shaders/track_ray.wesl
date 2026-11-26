// A track's ray follows the funnel wall from throat (uv.x = 0) to mouth
// (uv.x = 1). The pulse travels mouth to throat each beat of the track's clock.
//
// color  = [r, g, b, base_alpha] — track hue
// params = [beat_env, clock, beat_frac, _]

import bevy_pbr::render::forward_io::VertexOutput;

@group(constants::MATERIAL_BIND_GROUP) @binding(0) var<uniform> color: vec4<f32>;
@group(constants::MATERIAL_BIND_GROUP) @binding(1) var<uniform> params: vec4<f32>;

@fragment
fn fragment(in: VertexOutput) -> @location(0) vec4<f32> {
    let beat_env = params.x;
    let clock = params.y; // 0.0 stopped, 0.5 playing without a phasor, 1.0 live
    let beat_frac = params.z;

    // Soft filament across the width: bright core line, gaussian falloff.
    let across = abs(in.uv.y - 0.5) * 2.0;            // 0 at core → 1 at edge
    let core = exp(-across * across * 9.0);

    // Fade the beam's ends: melt into the throat glow, taper at the mouth.
    let ends = smoothstep(0.0, 0.06, in.uv.x) * (1.0 - smoothstep(0.94, 1.0, in.uv.x));

    // A brighter steady ray marks a track that reports playing.
    let base = (0.22 + clock * 0.30) * core * ends;
    var col = color.rgb * base;
    var alpha = color.a * base;

    // The pulse spans enough of the ray to read at room scale. Its center
    // travels between the ends of the taper so the onset can reach full brightness.
    if (clock > 0.75) {
        let pos = mix(0.94, 0.06, beat_frac);
        let d = in.uv.x - pos;
        let packet = exp(-d * d * 150.0) * core * ends;
        let hot = packet * (0.5 + beat_env * 2.6);
        col += color.rgb * hot * 2.4;
        alpha = max(alpha, min(hot, 1.0));
    }

    return vec4<f32>(col, alpha);
}
