// Chord Shader — a patch-bay wire ribbon bowing around the open center.
//
// The ribbon's uv.x runs SOURCE (0) → DEST (1) along the chord, uv.y crosses its
// width (see `view::patch_bay::ribbon_mesh`). A live wire is a solid body of
// light in its fabric hue, pushed HDR (>1.0) so the shared bloom pass halos it —
// "bright is live". When the app's render port sends MIDI a bright packet rides
// the chord src→dest: the traffic pulse. It is driven purely by `globals.time`
// against a per-pulse timestamp uniform (params.y) — one CPU write per pulse, the
// packet animates on the GPU (docs/scenes/patchbay.md, the live layer). A wire
// the app can't observe never gets a fresh timestamp, so it stays solid-lit.
//
// color  = [r, g, b, _]  — wire hue (HDR)
// params = [selected, pulse_time, _, _]
// tune   = [travel, band_width, pulse_gain, selected_gain]

#import bevy_pbr::forward_io::VertexOutput
#import bevy_pbr::mesh_view_bindings::globals

@group(#{MATERIAL_BIND_GROUP}) @binding(0) var<uniform> color: vec4<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(1) var<uniform> params: vec4<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(2) var<uniform> tune: vec4<f32>;

@fragment
fn fragment(in: VertexOutput) -> @location(0) vec4<f32> {
    let selected = params.x;
    let pulse_time = params.y;
    let travel = tune.x;
    let band_width = tune.y;
    let pulse_gain = tune.z;
    let selected_gain = tune.w;

    // Soft core across the ribbon's width: bright centre line, gentle falloff.
    let across = abs(in.uv.y - 0.5) * 2.0;   // 0 at core → 1 at either edge
    let core = 1.0 - across * across;

    // Idle glow: the wire's hue, lifted when it's the inspected chord.
    let gain = mix(1.0, selected_gain, selected);
    var col = color.rgb * gain * (0.55 + 0.45 * core);

    // The traveling traffic pulse: age∈[0,travel] ⇒ a gaussian packet centred at
    // uv.x = progress, riding source→dest. Faded in at the source and out at the
    // dest so it doesn't pop on/off at the seats. HDR so it blooms.
    let age = globals.time - pulse_time;
    if (age >= 0.0 && age <= travel) {
        let progress = age / travel;
        let d = in.uv.x - progress;
        let packet = exp(-(d * d) / max(band_width * band_width, 1e-5));
        let ends = smoothstep(0.0, 0.08, progress) * (1.0 - smoothstep(0.9, 1.0, progress));
        col += color.rgb * packet * ends * pulse_gain * core;
    }

    return vec4<f32>(col, 1.0);
}
