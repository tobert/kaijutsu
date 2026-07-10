// Trace Glow Shader — a faint, slowly moving glow for room decoration
// (`docs/scenes/shell.md`; `shaders::TraceGlowMaterial` has the full uniform
// layout doc). Zero per-frame CPU writes: every element's phase/rate/trough
// is baked in at spawn time, and all motion here comes from `globals.time`.
//
// color  = [r, g, b, _]      — crest hue, renormalized so its brightest
//                              channel is exactly GLOW_CREST (may exceed 1.0
//                              — HDR, blooms through the app's bloom pass)
// params = [phase, rate, trough, mode]
//   mode 0 — traveling wave: one crest glides along uv.x at `rate`
//     cycles/second, offset by `phase` (a `[0,1)` cycle-fraction). uv.x is
//     the element's own length fraction (`bearing::ribbon_vertices`'
//     cumulative arclength for floor traces, `room::glow_quad_mesh`'s
//     length-tracking UV for wall trim) — ONE wrap baked in (a single crest
//     per element; multiple wraps would need `uv.x * wraps` before the
//     fract() below, not needed by any element today).
//   mode 1 — breathing: uniform sine pulse, ignores uv entirely — safe on
//     primitives with their own UV convention (Torus, Annulus) this
//     material never has to match.

#import bevy_pbr::forward_io::VertexOutput
#import bevy_pbr::mesh_view_bindings::globals

@group(#{MATERIAL_BIND_GROUP}) @binding(0) var<uniform> color: vec4<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(1) var<uniform> params: vec4<f32>;

/// Width of the traveling crest as a fraction of the wave's cycle (mode 0)
/// — a narrow band so the glow reads as one crest passing through, not a
/// wash. **Amy-tunable** (a WGSL const, not a uniform, since the mission's
/// uniform layout is fixed at `[color, params]` — edit + rebuild to retune).
const BAND_WIDTH: f32 = 0.18;

@fragment
fn fragment(in: VertexOutput) -> @location(0) vec4<f32> {
    let phase = params.x;
    let rate = params.y;
    let trough = params.z;
    let mode = params.w;

    var brightness: f32;
    if (mode < 0.5) {
        // Traveling wave: the crest's center glides along uv.x at `rate`
        // cycles/second, wrapping with fract(). `d` is the wrapped distance
        // from this fragment to the nearest crest (handles the wrap at both
        // uv.x = 0 and uv.x = 1 — the crest passing the seam reads as one
        // continuous pass, not a jump).
        let cycle = fract(in.uv.x - globals.time * rate + phase);
        let d = min(cycle, 1.0 - cycle);
        let bump = 1.0 - smoothstep(0.0, BAND_WIDTH * 0.5, d);
        brightness = trough + (1.0 - trough) * bump;
    } else {
        // Breathing: uniform, slow sine — every fragment of the element in
        // lockstep (a ring, a pad, a bezel breathes as one body).
        let breath = 0.5 * (1.0 + sin(globals.time * rate + phase));
        brightness = trough + (1.0 - trough) * breath;
    }

    return vec4<f32>(color.rgb * brightness, 1.0);
}
