// Well Rings Shader — the time well's base "deck" and pulse.
//
// A flat disc behind the cards (XY plane, camera looks down −Z so it reads
// face-on like the concept art, mockups 27/33). It draws:
//   • concentric rings whose brightness + outward flow speed scale with `energy`,
//   • a spiral core at the center that spins faster as `energy` rises,
//   • localized ripples — expanding wavefronts fired at the angle of whichever
//     context just did something (the "localize to context angle" behavior).
// Bright parts are emitted **HDR** (>1.0) so the single-camera bloom pass blooms
// them into a glow (see `main::setup_camera`); the disc fades to nothing at its
// rim so the square quad's corners melt into the well background.
//
// energy   = [energy, _, _, _]
// ripples  = [cos(angle), sin(angle), age_norm (0..1), intensity] × MAX_RIPPLES

#import bevy_pbr::forward_io::VertexOutput
#import bevy_pbr::mesh_view_bindings::globals

const MAX_RIPPLES: u32 = 8u;
const PI: f32 = 3.14159265;

@group(#{MATERIAL_BIND_GROUP}) @binding(0) var<uniform> energy_v: vec4<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(1) var<uniform> core_color: vec4<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(2) var<uniform> ring_color: vec4<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(3) var<uniform> ripples: array<vec4<f32>, 8>;

@fragment
fn fragment(in: VertexOutput) -> @location(0) vec4<f32> {
    let energy = energy_v.x;
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

    // --- Concentric rings: sinusoidal bands flowing outward, faster when busy ---
    let flow = t * (0.35 + energy * 1.3);
    let ring_wave = 0.5 + 0.5 * sin(r * 26.0 - flow);
    // Sharpen into thin bright bands.
    let bands = pow(ring_wave, 3.0);
    let ring_bright = bands * (0.10 + energy * 0.9) * rim_fade;
    col += ring_color.rgb * ring_bright;

    // --- Accretion-disk event horizon: a hot glowing ring around a dark throat
    // (the singularity the haystack falls into), with log-spiral arms feeding in
    // and a rotating Doppler-bright side. HDR so it blooms into a soft, hot glow;
    // brighter + faster with system energy. ---
    let spin = t * (0.5 + energy * 2.0);
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
    let core_bright = disk * (1.6 + energy * 3.0) + singularity * (1.2 + energy * 2.0);
    col += core_color.rgb * core_bright;

    // --- Ripples: expanding wavefronts at each context's angle ---
    var ripple_bright = 0.0;
    for (var i = 0u; i < MAX_RIPPLES; i = i + 1u) {
        let rp = ripples[i];
        let intensity = rp.w;
        if (intensity <= 0.001) {
            continue;
        }
        let age = rp.z;                                // 0..1 (age / lifetime)
        let front = age;                               // wavefront radius
        // Thin gaussian wavefront, widening a touch as it travels.
        let width = 0.045 + age * 0.05;
        let radial = exp(-pow((r - front) / width, 2.0));
        // Angular window around the ripple's direction (dot of unit vectors).
        let rdir = vec2<f32>(rp.x, rp.y);
        let align = dot(dir, rdir);                    // 1 = same direction
        let ang = smoothstep(0.25, 0.92, align);
        let fade = 1.0 - age;                          // dim as it expands
        ripple_bright += radial * ang * intensity * fade;
    }
    // Cyan-white wavefronts, HDR.
    col += vec3<f32>(0.5, 0.95, 1.0) * ripple_bright * 2.6 * rim_fade;

    // Alpha: visible where there's light, plus a faint base wash so the deck
    // exists even when idle; everything fades at the rim.
    let lum = ring_bright + core_bright + ripple_bright;
    let alpha = clamp(0.06 + lum * 1.2, 0.0, 0.95) * rim_fade;
    return vec4<f32>(col, alpha);
}
