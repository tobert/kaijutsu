// Heroic view sun disc (`docs/newui.md`, "Sun and sky").
//
// Vertical 3-stop gradient (`sun_top`, `sun_mid`, `sun_bot`) with seven
// transparent horizontal bands cut across its lower half, thickening toward
// the base so the sky backdrop shows through. Driven HDR (>1.0) so it blooms.
// Takes no distance fog.

import bevy_pbr::render::forward_io::VertexOutput;
import bevy_pbr::render::mesh_view_bindings::globals;

@group(constants::MATERIAL_BIND_GROUP) @binding(0) var<uniform> sun_top: vec4<f32>;
@group(constants::MATERIAL_BIND_GROUP) @binding(1) var<uniform> sun_mid: vec4<f32>;
@group(constants::MATERIAL_BIND_GROUP) @binding(2) var<uniform> sun_bot: vec4<f32>;
/// `[intensity, beat_pulse, reduced_motion, _]`
@group(constants::MATERIAL_BIND_GROUP) @binding(3) var<uniform> params: vec4<f32>;

@fragment
fn fragment(in: VertexOutput) -> @location(0) vec4<f32> {
    // UV in [0, 1]^2 over a Rectangle quad (y=0 at top, y=1 at bottom).
    let centered = in.uv * 2.0 - vec2<f32>(1.0, 1.0);
    let r = length(centered);
    if (r > 1.0) {
        discard;
    }

    // Soft outer AA edge.
    let edge = 1.0 - smoothstep(0.975, 1.0, r);

    // Height fraction: 1.0 at top of disc, 0.0 at base.
    let h = clamp(1.0 - in.uv.y, 0.0, 1.0);

    // Three-stop vertical gradient: top -> mid -> base.
    var col: vec3<f32>;
    if (h > 0.5) {
        col = mix(sun_mid.rgb, sun_top.rgb, (h - 0.5) * 2.0);
    } else {
        col = mix(sun_bot.rgb, sun_mid.rgb, h * 2.0);
    }

    // Seven horizontal bands across the visible lower half (h in [0.46, 0.74]),
    // thickening toward the horizon so the sky backdrop shows through.
    if (h < 0.74 && h >= 0.46) {
        let lower_t = (0.74 - h) / 0.28; // 0 at top of band, 1 at base
        let drift = select(fract(globals.time * 0.06), 0.0, params.z > 0.5);
        let band_pos = fract(lower_t * 7.0 - drift);
        let stripe_width = mix(0.10, 0.52, pow(lower_t, 1.25));
        if (band_pos < stripe_width) {
            discard;
        }
    }

    // Push above the 1.0 bloom threshold scaled subtly by energy and beat.
    let boost = 1.25 + params.x * 0.45 + params.y * 0.15;
    return vec4<f32>(col * boost, edge);
}
