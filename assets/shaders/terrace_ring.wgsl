// Terrace Ring Shader — magic-circle glyph rings at the time well's terrace
// boundaries (the Konosuba/"Explosion"-spell aesthetic).
//
// A flat annulus quad, camera-facing like the well rings deck (XY plane,
// camera looks down −Z so it reads face-on). Draws, additively/emissive:
//   • the annulus band itself, masked to [inner_radius_frac, outer_radius_frac]
//     (transparent inside so deeper rings show through, transparent at the
//     quad's corners so the square never reads as a square),
//   • thin bright rim lines at the band's inner/outer edges,
//   • a thin concentric mid-line,
//   • N radial tick marks (spokes) spinning at `spin_rate * spin_dir`.
// Bright parts are emitted **HDR** (>1.0) so the single-camera bloom pass
// blooms them into a glow (see `main::setup_camera`).
//
// params = [inner_radius_frac, outer_radius_frac, spin_rate, spin_dir]
// color  = glyph color, linear rgb in .xyz (HDR-scaled below), .w = alpha/intensity

#import bevy_pbr::forward_io::VertexOutput
#import bevy_pbr::mesh_view_bindings::globals

const PI: f32 = 3.14159265;
const N_TICKS: f32 = 24.0;

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

    // Rotate the angular coordinate over time — this is the whole "spin": the
    // tick marks (and everything angle-dependent) sweep around the ring.
    var theta = atan2(p.y, p.x);
    theta = theta + globals.time * spin_rate * spin_dir;

    // --- Annulus band mask: 0 inside `inner`, 1 through the band, 0 past `outer` ---
    let edge_soft = 0.015;
    let band = smoothstep(inner - edge_soft, inner + edge_soft, r)
        * (1.0 - smoothstep(outer - edge_soft, outer + edge_soft, r));

    // --- Thin bright rim lines at the band's inner/outer edges ---
    let rim_width = 0.008;
    let inner_rim = 1.0 - smoothstep(0.0, rim_width, abs(r - inner));
    let outer_rim = 1.0 - smoothstep(0.0, rim_width, abs(r - outer));

    // --- One thin concentric mid-line, centered in the band ---
    let mid_r = (inner + outer) * 0.5;
    let mid_rim = (1.0 - smoothstep(0.0, rim_width * 0.75, abs(r - mid_r))) * band;

    // --- N radial tick marks (spokes) across the band ---
    let wedge = fract(theta / (2.0 * PI) * N_TICKS);
    let tick_half_width = 0.05;
    let tick_dist = min(wedge, 1.0 - wedge);
    let ticks = (1.0 - smoothstep(0.0, tick_half_width, tick_dist)) * band;

    // --- Composite + HDR scale so it blooms ---
    let glyph = band * 0.30 + ticks * 0.85 + inner_rim + outer_rim * 0.85 + mid_rim * 0.6;
    let hdr_scale = 3.0;
    let col = color.rgb * glyph * hdr_scale;

    let alpha = clamp(band * 0.5 + ticks + inner_rim + outer_rim + mid_rim, 0.0, 1.0) * color.w;
    return vec4<f32>(col, alpha);
}
