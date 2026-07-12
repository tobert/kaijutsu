// Terrace Ring Shader — ornate magic-circle glyph rings at the time well's
// terrace boundaries (the Konosuba/"Explosion"-spell aesthetic).
//
// A flat annulus quad, camera-facing like the well rings deck (XY plane,
// camera looks down −Z so it reads face-on). Draws, additively/emissive, an
// ornate summoning-glyph grid:
//   • the annulus band (transparent inside so deeper rings show through,
//     transparent at the quad's corners so the square never reads as a square),
//   • thin bright rim lines at the band's inner/outer edges,
//   • N_CONCENTRIC evenly-spaced sub-rings inside the band (a radial grid),
//   • a two-tier radial spoke grid — N_MAJOR bright/long spokes + N_MINOR
//     dim/short spokes — so the band reads as grid cells,
//   • a CENTERPIECE inside the inner circle: each ring draws ONE variant from
//     a four-strong family (ring_index % 4 — adjacent layers always differ):
//       0 barcode graduations + counter-spinning vernier (mouth ring)
//       1 harmonic rosette — braided wave rings, gold nodes
//       2 Fibonacci moiré dial — 13/21 coprime tick rings, one counter-spun
//       3 orbiting motes — gold satellites with comet trails (throat ring)
//     plus, on EVERY ring, a gem-glint layer: seeded sites that pulse gold
//     HDR on smooth per-site phases (sparkle without frame-random flicker).
//     Everything is a pure function of (θ, r, ring identity, globals.time) —
//     deterministic per ring, stable across frames, zero per-frame CPU, and
//     no straight chords / star topology (the hexagram stays dead).
//     Two-tone discipline: lines speak the ring's violet; picks/nodes/motes/
//     glints mix toward the room's gold (accent uniform = palette::GOLD_HUE).
// Bright parts are emitted **HDR** (>1.0) so the single-camera bloom pass
// blooms them into a glow (see `main::setup_camera`).
//
// params = [inner_radius_frac, outer_radius_frac, spin_rate, spin_dir]
// color  = glyph color, linear rgb in .xyz (HDR-scaled below), .w = alpha/intensity

#import bevy_pbr::forward_io::VertexOutput
#import bevy_pbr::mesh_view_bindings::globals

const TAU: f32 = 6.28318530;

// ── Grid-density knobs (tunable) ────────────────────────────────────────────
// Concentric sub-rings drawn *inside* the band (between the inner/outer rims).
const N_CONCENTRIC: f32 = 3.0;
// Major radial spokes: bright + full-band length (every 360/N_MAJOR degrees).
const N_MAJOR: f32 = 12.0;   // 12 → every 30°
// Minor radial spokes: dim + short (every 360/N_MINOR degrees).
const N_MINOR: f32 = 48.0;   // 48 → every 7.5°
// HDR emissive multiplier so the glyph blooms.
const HDR_SCALE: f32 = 3.0;

// ── Centerpiece family (all knobs Amy-tunable) ──────────────────────────────
// Each ring draws variant `ring_index % N_VARIANTS`. Set GLYPH_FORCE to 0..3
// to force one variant onto every ring for tuning; -1 = the per-ring mix.
const N_VARIANTS: u32 = 4u;
const GLYPH_FORCE: i32 = -1;
// Variant 0 — barcode: main dial segment count, vernier tick count, and the
// hash threshold above which a segment renders gold (0.78 → ~22% of segments).
const N_BARCODE: f32 = 40.0;
const N_VERNIER: f32 = 96.0;
const BARCODE_GOLD_PICK: f32 = 0.78;
// Variant 1 — rosette: wave amplitude as a fraction of the inner radius.
const ROSETTE_EPS_FRAC: f32 = 0.045;
// Variant 2 — moiré dial: two coprime tick frequencies (Fibonacci neighbors —
// the beat pattern never visually repeats around the circle).
const MOIRE_A: f32 = 13.0;
const MOIRE_B: f32 = 21.0;
// Variant 3 — motes: orbiting satellite count.
const N_MOTES: u32 = 6u;
// Gem-glint layer (every ring): site count, twinkle sharpness (higher =
// briefer glints), and gold gain.
const N_GEMS: u32 = 8u;
const GEM_SHARP: f32 = 14.0;
const GEM_GAIN: f32 = 1.4;

// Stable pseudo-random in [0,1) from a float key. Only ever fed per-ring /
// per-segment constants, so patterns are deterministic and frame-stable.
fn hash01(x: f32) -> f32 {
    return fract(sin(x * 12.9898 + 78.233) * 43758.5453);
}

// Wrap a value in "turns" to [-0.5, 0.5) — nearest angular offset.
fn wrap_half(x: f32) -> f32 {
    return fract(x + 0.5) - 0.5;
}

@group(#{MATERIAL_BIND_GROUP}) @binding(0) var<uniform> params: vec4<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(1) var<uniform> color: vec4<f32>;
// [ring_index, ring_count, 0, 0] — ring identity (variant pick + hash seeds).
@group(#{MATERIAL_BIND_GROUP}) @binding(2) var<uniform> glyph: vec4<f32>;
// Accent (the room's gold, palette::GOLD_HUE) — the centerpiece's second tone.
@group(#{MATERIAL_BIND_GROUP}) @binding(3) var<uniform> accent: vec4<f32>;

@fragment
fn fragment(in: VertexOutput) -> @location(0) vec4<f32> {
    let inner = params.x;
    let outer = params.y;
    let spin_rate = params.z;
    let spin_dir = params.w;

    // Centered coords in [-1, 1], +y up (uv.y runs top-down, flip it) — same
    // convention as `well_rings.wgsl` so this ring's angle reads consistent
    // with the deck's.
    let p = vec2<f32>(in.uv.x - 0.5, 0.5 - in.uv.y) * 2.0;
    let r = length(p);

    // Outside the quad's inscribed unit circle: nothing (corners vanish).
    if (r > 1.0) {
        return vec4<f32>(0.0, 0.0, 0.0, 0.0);
    }

    // Rotate the angular coordinate over time — the whole "spin": every
    // angle-dependent feature (spokes, dashed arc) sweeps around the ring.
    let spin = globals.time * spin_rate * spin_dir;
    let theta = atan2(p.y, p.x) + spin;

    let mid_r = (inner + outer) * 0.5;

    // --- Annulus band mask: 0 inside `inner`, 1 through the band, 0 past `outer` ---
    let edge_soft = 0.015;
    let band = smoothstep(inner - edge_soft, inner + edge_soft, r)
        * (1.0 - smoothstep(outer - edge_soft, outer + edge_soft, r));

    // --- Thin bright rim lines at the band's inner/outer edges ---
    let rim_width = 0.008;
    let inner_rim = 1.0 - smoothstep(0.0, rim_width, abs(r - inner));
    let outer_rim = 1.0 - smoothstep(0.0, rim_width, abs(r - outer));

    // --- N_CONCENTRIC evenly-spaced sub-rings inside the band ---
    // Sub-ring j sits at inner + (j+1)/(N_CONCENTRIC+1) of the band width, so
    // they're evenly spaced strictly between the two rims.
    var concentric = 0.0;
    let band_w = outer - inner;
    for (var j = 0u; j < u32(N_CONCENTRIC); j = j + 1u) {
        let frac = (f32(j) + 1.0) / (N_CONCENTRIC + 1.0);
        let sub_r = inner + band_w * frac;
        concentric += (1.0 - smoothstep(0.0, rim_width * 0.75, abs(r - sub_r)));
    }
    concentric *= band;

    // --- Two-tier radial spoke grid ---
    // Major: bright, span the whole band. `fract` of angle*N is a sawtooth; the
    // nearest-edge distance is a thin line at each spoke.
    let major_w = 0.045;
    let mw = fract(theta / TAU * N_MAJOR);
    let major = (1.0 - smoothstep(0.0, major_w, min(mw, 1.0 - mw))) * band;
    // Minor: dim, short — fades out past mid-band so it reads as inner grid ticks.
    let minor_w = 0.06;
    let nw = fract(theta / TAU * N_MINOR);
    let minor_band = band * (1.0 - smoothstep(mid_r, outer, r));
    let minor = (1.0 - smoothstep(0.0, minor_w, min(nw, 1.0 - nw))) * minor_band;

    // --- Centerpiece inside the inner circle: per-ring variant + gem glints ---
    // `cp_base` accumulates weight on the ring's own hue, `cp_gold` on the
    // accent — the two tones of every centerpiece.
    var cp_base = 0.0;
    var cp_gold = 0.0;
    if (r < inner) {
        let ring_index = glyph.x;
        let seed = ring_index * 17.0 + 3.0;
        var variant = u32(ring_index) % N_VARIANTS;
        if (GLYPH_FORCE >= 0) {
            variant = u32(GLYPH_FORCE);
        }
        let arc_r = inner * 0.9;                     // radius the retired dial occupied
        let dash_line = 0.02 * inner;                // line width, scaled to ring size
        // Ring-local angle in [0,1): indexing through `fract(theta/TAU)` (not
        // raw theta) keeps hashed patterns fixed to the ring across full
        // revolutions — raw theta would re-roll them every turn.
        let ang01 = fract(theta / TAU);

        if (variant == 0u) {
            // Barcode graduations: per-segment duty + brightness hashed from
            // (ring seed, segment index); hash-picked segments render gold.
            let ring_line = 1.0 - smoothstep(0.0, dash_line, abs(r - arc_r));
            let segf = ang01 * N_BARCODE;
            let seg_i = floor(segf);
            let seg_phase = fract(segf) - 0.5;
            let duty = mix(0.15, 0.85, hash01(seg_i + seed * 7.0));
            let bright = mix(0.45, 1.0, hash01(seg_i * 1.618 + seed * 3.1));
            let dash_edge = 0.05;
            let bar = 1.0 - smoothstep(duty * 0.5 - dash_edge, duty * 0.5 + dash_edge, abs(seg_phase));
            let bar_seg = ring_line * bar * bright;
            let gold_pick = step(BARCODE_GOLD_PICK, hash01(seg_i * 3.7 + seed * 5.0)) * 0.55;
            // Vernier: a finer tick ring further in, counter-spinning (theta
            // minus twice the baked spin = net reverse), with hash-dropped
            // teeth so each ring's fine scale is recognizably its own.
            let vern_r = inner * 0.76;
            let vern_line = 1.0 - smoothstep(0.0, dash_line * 0.75, abs(r - vern_r));
            let vf = fract((theta - 2.0 * spin) / TAU) * N_VERNIER;
            let v_i = floor(vf);
            let v_phase = fract(vf) - 0.5;
            let tooth = step(0.15, hash01(v_i + seed * 11.0));
            let tick = 1.0 - smoothstep(0.12, 0.22, abs(v_phase));
            cp_base += bar_seg * (1.0 - gold_pick) + vern_line * tick * tooth * 0.5;
            cp_gold += bar_seg * gold_pick;
        } else if (variant == 1u) {
            // Harmonic rosette: two mirrored wave rings r0 ± ε·cos(kθ) braid
            // around the mean circle; crossings (the wave's zeros) get a gold
            // node highlight. k is an odd lobe count hashed per ring — integer
            // k keeps cos continuous across the θ wrap, odd avoids star
            // symmetry.
            let rose_r = inner * 0.84;
            let eps = inner * ROSETTE_EPS_FRAC;
            let k_lobes = 5.0 + 2.0 * floor(hash01(seed * 31.0) * 3.999);
            let w = cos(k_lobes * theta);
            let line_w = 0.018 * inner;
            let rose1 = 1.0 - smoothstep(0.0, line_w, abs(r - (rose_r + eps * w)));
            let rose2 = 1.0 - smoothstep(0.0, line_w, abs(r - (rose_r - eps * w)));
            let node = (1.0 - smoothstep(0.0, 0.15, abs(w)))
                * (1.0 - smoothstep(0.0, line_w * 1.5, abs(r - rose_r)));
            cp_base += max(rose1, rose2 * 0.6);
            cp_gold += node * 0.9;
        } else if (variant == 2u) {
            // Fibonacci moiré dial: two tick rings at coprime frequencies
            // (13/21) on the same radius, one spinning with the glyph, one
            // counter-spun — the interference pattern walks around the dial
            // without ever visually repeating. Coincident ticks flash gold.
            let tick_len = 0.10 * inner;
            let radial = 1.0 - smoothstep(tick_len * 0.55, tick_len, abs(r - arc_r));
            let fa = fract(ang01 * MOIRE_A);
            let ta = 1.0 - smoothstep(0.0, 0.06, min(fa, 1.0 - fa));
            let fb = fract(fract((theta - 2.0 * spin) / TAU) * MOIRE_B);
            let tb = 1.0 - smoothstep(0.0, 0.05, min(fb, 1.0 - fb));
            cp_base += radial * (ta * 0.85 + tb * 0.35);
            cp_gold += radial * (tb * 0.45 + ta * tb * 1.2);
        } else {
            // Orbiting motes: gold satellites circling the centerpiece radius
            // at hashed speeds/directions, each towing a short comet trail.
            // Computed in the UNSPUN angle so their orbits are their own
            // motion, not the ring's.
            let theta_raw = atan2(p.y, p.x);
            var mote = 0.0;
            for (var m = 0u; m < N_MOTES; m = m + 1u) {
                let fm = f32(m);
                let a0 = hash01(seed * 3.0 + fm * 9.13);
                let sp = mix(0.03, 0.09, hash01(seed * 7.0 + fm * 4.71));
                let dir = select(-1.0, 1.0, hash01(seed * 11.0 + fm * 6.2) > 0.5);
                let am = a0 + globals.time * sp * dir;   // turns
                // Wobble capped so dot + wobble stay inside the r<inner gate.
                let rm = arc_r * (1.0 + 0.06 * sin(globals.time * 0.6 + fm * 2.1));
                let dphi = wrap_half(theta_raw / TAU - am);
                let dx = dphi * TAU * rm;
                let dy = r - rm;
                let dot_r = 0.045 * inner;
                let core = exp(-(dx * dx + dy * dy) / (dot_r * dot_r));
                let behind = dphi * TAU * -dir;          // >0 = trailing side
                let trail = select(0.0, exp(-behind * 3.0), behind > 0.0)
                    * (1.0 - smoothstep(0.0, dot_r * 1.6, abs(dy)));
                mote += core + trail * 0.35;
            }
            cp_gold += mote * 0.9;
            cp_base += mote * 0.25;
        }

        // Gem glints, every ring: seeded sites near the centerpiece radius,
        // fixed to the spinning ring frame, each pulsing gold on its own
        // smooth phase — sharpened sine, so glints are brief but never
        // frame-random.
        var gem = 0.0;
        for (var j = 0u; j < N_GEMS; j = j + 1u) {
            let fj = f32(j);
            let aj = hash01(seed * 5.0 + fj * 7.31);
            let rj = arc_r + (hash01(seed * 9.0 + fj * 3.77) - 0.5) * 0.12 * inner;
            let dphi = wrap_half(ang01 - aj);
            let dx = dphi * TAU * rj;
            let dy = r - rj;
            let dot_r = 0.035 * inner;
            let core = exp(-(dx * dx + dy * dy) / (dot_r * dot_r));
            let wj = mix(0.4, 1.1, hash01(seed * 13.0 + fj * 5.9));
            let phj = hash01(seed * 21.0 + fj * 11.17) * TAU;
            let tw = pow(0.5 + 0.5 * sin(globals.time * wj + phj), GEM_SHARP);
            gem += core * tw;
        }
        cp_gold += gem * GEM_GAIN;
        cp_base += gem * 0.2;
    }

    // --- Composite + HDR scale so it blooms ---
    // Band furniture speaks the ring's hue; the centerpiece adds its own
    // two-tone mix (base hue + gold accent).
    let glyph_mono = band * 0.18
        + concentric * 0.55
        + major * 0.75
        + minor * 0.35
        + inner_rim
        + outer_rim * 0.85;
    let col = (color.rgb * (glyph_mono + cp_base * 0.9)
        + accent.rgb * cp_gold * 0.9) * HDR_SCALE;

    let alpha_raw = band * 0.35
        + concentric * 0.6
        + major
        + minor * 0.5
        + inner_rim
        + outer_rim
        + cp_base
        + cp_gold;
    let alpha = clamp(alpha_raw, 0.0, 1.0) * color.w;
    return vec4<f32>(col, alpha);
}
