// The well deck draws concentric rings, a core lit by the loudest track beat,
// and ripples at the bearings of contexts whose turns ended.
// ripples = [cos(angle), sin(angle), radius (0..1), amplitude] × MAX_RIPPLES

import bevy_pbr::render::forward_io::VertexOutput;
import bevy_pbr::render::mesh_view_bindings::globals;

const MAX_RIPPLES: u32 = 8u;
const PI: f32 = 3.14159265;

@group(constants::MATERIAL_BIND_GROUP) @binding(0) var<uniform> beat: f32;
@group(constants::MATERIAL_BIND_GROUP) @binding(1) var<uniform> core_color: vec4<f32>;
@group(constants::MATERIAL_BIND_GROUP) @binding(2) var<uniform> ring_color: vec4<f32>;
@group(constants::MATERIAL_BIND_GROUP) @binding(3) var<uniform> ripples: array<vec4<f32>, 8>;

@fragment
fn fragment(in: VertexOutput) -> @location(0) vec4<f32> {
    let t = globals.time;

    // Centered, world-oriented coords in [-1, 1]: +y is up (uv.y runs top-down,
    // so flip it) to match the cards' world angle `atan2(card.y, card.x)`.
    let p = vec2<f32>(in.uv.x - 0.5, 0.5 - in.uv.y) * 2.0;
    let r = length(p);

    // Outside the unit disc: nothing (square corners vanish into the bg).
    if (r > 1.0) {
        return vec4<f32>(0.0, 0.0, 0.0, 0.0);
    }
    let dir = select(p / max(r, 1e-4), vec2<f32>(1.0, 0.0), r < 1e-4);
    let theta = atan2(p.y, p.x);

    // Soft inner/outer falloff so the deck reads as a bowl of light, not a slab,
    // and so the rim dissolves into the background.
    let rim_fade = 1.0 - smoothstep(0.78, 1.0, r);

    var col = vec3<f32>(0.0, 0.0, 0.0);

    // --- Concentric rings: sinusoidal bands flowing outward ---
    let flow = t * 0.35;
    let ring_wave = 0.5 + 0.5 * sin(r * 26.0 - flow);
    // Sharpen into thin bright bands.
    let bands = pow(ring_wave, 3.0);
    let ring_bright = bands * 0.10 * rim_fade;
    col += ring_color.rgb * ring_bright;

    // --- Accretion-disk event horizon: a hot glowing ring around a dark throat
    // (the singularity the haystack falls into), with log-spiral arms feeding in
    // and a rotating Doppler-bright side. ---
    let spin = t * 0.5;
    let eh = 0.12;                                      // event-horizon radius
    // Disk glow: rises just outside the horizon, decays outward; the hole stays dark.
    let edge = smoothstep(eh * 0.55, eh, r);            // 0 in the hole → 1 at the rim
    let falloff = exp(-(r - eh) * 6.5);                 // hot inner edge, fading out
    var disk = edge * falloff;
    // Log-spiral arms winding into the throat + a rotating Doppler-bright side.
    let arms = 0.6 + 0.4 * sin(theta * 2.0 - log(r + 0.03) * 7.0 - spin);
    let doppler = 0.55 + 0.45 * sin(theta - spin * 1.3);
    disk = disk * (0.45 + 0.55 * arms) * (0.6 + 0.4 * doppler);
    // A small hot pinprick right at the singularity.
    let singularity = exp(-r * r / (0.02 * 0.02));
    let core_bright = disk * (1.6 + beat * 2.2)
        + singularity * (1.2 + beat * 3.0);
    col += core_color.rgb * core_bright;

    // --- Ripples: expanding wavefronts at each context's angle ---
    var ripple_bright = 0.0;
    for (var i = 0u; i < MAX_RIPPLES; i = i + 1u) {
        let rp = ripples[i];
        let amplitude = rp.w;
        if (amplitude <= 0.001) {
            continue;
        }
        let front = rp.z;
        // Gaussian wavefront that widens as it travels.
        let width = 0.045 + front * 0.05;
        let radial = exp(-pow((r - front) / width, 2.0));
        // Angular window around the ripple's direction (dot of unit vectors).
        let rdir = vec2<f32>(rp.x, rp.y);
        let align = dot(dir, rdir);                    // 1 = same direction
        let ang = smoothstep(0.25, 0.92, align);
        ripple_bright += radial * ang * amplitude;
    }
    // Cyan-white wavefronts, HDR.
    col += ring_color.rgb * ripple_bright * 2.6 * rim_fade;

    // Alpha: visible where there's light, plus a faint base wash so the deck
    // exists even when idle; everything fades at the rim.
    let lum = ring_bright + core_bright + ripple_bright;
    let alpha = clamp(0.06 + lum * 1.2, 0.0, 0.95) * rim_fade;
    return vec4<f32>(col, alpha);
}
