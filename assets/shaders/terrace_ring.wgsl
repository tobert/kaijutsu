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
//   • a dashed inner ARC RING inside the inner circle — a segmented dial
//     line (etched instrument graduations) spinning with the same motion as
//     the rest of the glyph. Placeholder centerpiece pending a later glow-up.
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
// Dashed inner arc ring: number of dash segments around the circle, and the
// fraction of each segment that's "on" (the rest is the gap).
const N_DASH_ARC: f32 = 16.0;
const DASH_DUTY: f32 = 0.6;
// HDR emissive multiplier so the glyph blooms.
const HDR_SCALE: f32 = 3.0;

@group(#{MATERIAL_BIND_GROUP}) @binding(0) var<uniform> params: vec4<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(1) var<uniform> color: vec4<f32>;

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

    // --- Dashed inner arc: a segmented dial ring inside the inner circle ---
    // A thin circle at the same radius the old centerpiece occupied, broken
    // into N_DASH_ARC dashes by an angular duty-cycle mask — no straight
    // chords, so it can't read as star topology. Spins with the same `spin`
    // (baked into `theta` above) as the rest of the glyph.
    var dashed_arc = 0.0;
    if (r < inner) {
        let arc_r = inner * 0.9;                     // same radius the old glyph occupied
        let dash_line = 0.02 * inner;                // line width, scaled to ring size
        let ring_line = 1.0 - smoothstep(0.0, dash_line, abs(r - arc_r));
        // Segment-local phase in [-0.5, 0.5), centered on each dash so both
        // dash/gap transitions are softened (no hard edge at the wrap point).
        let dash_phase = fract(theta / TAU * N_DASH_ARC) - 0.5;
        let dash_edge = 0.04;
        let half_duty = DASH_DUTY * 0.5;
        let dash_mask = 1.0 - smoothstep(half_duty - dash_edge, half_duty + dash_edge, abs(dash_phase));
        dashed_arc = ring_line * dash_mask;
    }

    // --- Composite + HDR scale so it blooms ---
    let glyph = band * 0.18
        + concentric * 0.55
        + major * 0.75
        + minor * 0.35
        + inner_rim
        + outer_rim * 0.85
        + dashed_arc * 0.9;
    let col = color.rgb * glyph * HDR_SCALE;

    let alpha_raw = band * 0.35
        + concentric * 0.6
        + major
        + minor * 0.5
        + inner_rim
        + outer_rim
        + dashed_arc;
    let alpha = clamp(alpha_raw, 0.0, 1.0) * color.w;
    return vec4<f32>(col, alpha);
}
