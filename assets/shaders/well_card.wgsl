// Well Card Shader — 3D material for time-well cards (rim + focus).
//
// Draws the whole card on the GPU (vello-free): an accent rounded-rect body
// (SDF), the MSDF text composited on top, and the "bling" rings/pulses as SDF
// emitting **HDR** (>1.0) color so they spill into the camera's bloom pass (the
// app renders on one HDR `Camera3d`; see `main::setup_camera`). The bloom blur
// turns each bright rim into a glow halo that extends past the card silhouette —
// which the masked alpha alone could never do (rounded corners discard below the
// cutoff, so a soft falloff *inside* alpha is impossible; bloom does it instead).
//
// `params = [selected, in_lineage, status, _]`; `status`: 0/1 pending → 0,
// running → 1, done → 2, error → 3. Animation reads `globals.time` directly.

#import bevy_pbr::forward_io::VertexOutput
#import bevy_pbr::mesh_view_bindings::globals

@group(#{MATERIAL_BIND_GROUP}) @binding(0) var card_texture: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(1) var card_sampler: sampler;
@group(#{MATERIAL_BIND_GROUP}) @binding(2) var<uniform> accent: vec4<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(3) var<uniform> params: vec4<f32>; // [selected, in_lineage, status, drifting]
@group(#{MATERIAL_BIND_GROUP}) @binding(4) var<uniform> shape: vec4<f32>;  // [aspect, corner_radius, ring_width, inset]
@group(#{MATERIAL_BIND_GROUP}) @binding(5) var<uniform> border: vec4<f32>; // [r, g, b, strength] — steady outline (HUD); cards leave strength 0
@group(#{MATERIAL_BIND_GROUP}) @binding(6) var<uniform> dim: vec4<f32>;    // [brightness, chatter, beat, _] — focus dim ×, live chatter glow, beat envelope

// Signed distance to a rounded box centered at origin, half-size `b`, radius `r`.
fn sd_round_box(p: vec2<f32>, b: vec2<f32>, r: f32) -> f32 {
    let q = abs(p) - b + vec2<f32>(r, r);
    return length(max(q, vec2<f32>(0.0, 0.0))) + min(max(q.x, q.y), 0.0) - r;
}

@fragment
fn fragment(in: VertexOutput) -> @location(0) vec4<f32> {
    let aspect = shape.x;
    let radius = shape.y;
    let ring_w = shape.z;
    let inset = shape.w;

    // Aspect-corrected, centered coords so corners stay circular on a wide quad.
    let pc = vec2<f32>((in.uv.x - 0.5) * aspect, in.uv.y - 0.5);
    let b = vec2<f32>(aspect * 0.5 - inset, 0.5 - inset);
    let d = sd_round_box(pc, b, radius);

    let aa = fwidth(d) + 1e-4;
    let inside = 1.0 - smoothstep(0.0, aa, d);

    let t = globals.time;

    // Accent body.
    var col = accent.rgb;
    var alpha = accent.a * inside;

    // Drift sheen (under the text): a narrow band sweeping diagonally across the
    // body when this context is a staged-drift endpoint. Kept **LDR** (<1.0,
    // muted) so it does NOT bloom — staged drift is *passive* structural state,
    // not action, and the bright/blooming vocabulary is reserved for live system
    // activity (the base ring deck + the Running rim). (params.w = drifting.)
    if (params.w > 0.5) {
        let phase = fract((pc.x + pc.y) * 0.9 - t * 0.5);
        let sheen = smoothstep(0.46, 0.50, phase) - smoothstep(0.50, 0.54, phase);
        col += vec3<f32>(0.30, 0.42, 0.55) * sheen * 0.5 * inside;
    }

    // MSDF text on top (text texture is transparent except glyphs).
    let text = textureSample(card_texture, card_sampler, in.uv);
    col = mix(col, text.rgb, text.a);
    alpha = max(alpha, text.a * inside);

    // Ring band hugging the inner edge of the rounded box. HDR colors below push
    // the band well past 1.0 so the bloom pass blooms it into a glow halo.
    let band = (1.0 - smoothstep(ring_w, ring_w + aa, abs(d))) * inside;

    // Steady border (HUD panels: a glowing outline framing an empty interior, so
    // panels read as panels without a body fill). Cards leave `border.a == 0`.
    if (border.a > 0.001) {
        col = mix(col, border.rgb, band * border.a);
        alpha = max(alpha, band * border.a);
    }

    let status = params.z;

    // Live-action signals accumulate here and are added AFTER the focus dim:
    // "bright = live action" must read from every ring, not just the focused
    // one — a running/beating/chattering card on a dimmed ring is exactly the
    // at-a-glance signal the well exists for (Gemini review, 2026-07-04).
    // Navigation state (selection/lineage) and passive structure stay under
    // the dim so the focused ring still pops.
    var live = vec3<f32>(0.0, 0.0, 0.0);

    // --- Live chatter (dim.y): the context's decaying event energy, pushed by
    // the kernel-wide block stream the moment this card's context is talking —
    // a cyan HDR lift on the rim that fades in ~2s of quiet. Sits UNDER the
    // status/selection rims (it's ambience, not state). ---
    let chatter = dim.y;
    if (chatter > 0.005) {
        live += vec3<f32>(0.45, 0.85, 1.0) * (2.2 * chatter) * band;
        alpha = max(alpha, band * min(chatter * 2.0, 1.0));
    }

    // --- Beat (dim.z): the live beat envelope of this card's track phasor
    // (score-context cards today; every attached card once the track roster is
    // on the wire). A warm gold thump, HDR so it blooms on the beat. ---
    let beat = dim.z;
    if (beat > 0.005) {
        live += vec3<f32>(1.0, 0.72, 0.22) * (2.8 * beat) * band;
        alpha = max(alpha, band * min(beat * 2.0, 1.0));
    }

    // --- Status rim (base layer; selection/lineage draw over it) ---
    if (status > 2.5) {
        // Error: a steady, hot red rim.
        live += vec3<f32>(1.0, 0.16, 0.12) * 3.5 * band;
        alpha = max(alpha, band);
    } else if (status > 0.5 && status < 1.5) {
        // Running: a breathing teal rim — the "this context is thinking" pulse.
        let pulse = 0.5 + 0.5 * sin(t * 4.0);
        live += vec3<f32>(0.40, 0.95, 0.80) * (1.6 + 2.6 * pulse) * band * (0.45 + 0.55 * pulse);
        alpha = max(alpha, band);
    }

    // --- Lineage ring (amber, HDR) ---
    if (params.y > 0.5) {
        col = mix(col, vec3<f32>(0.95, 0.70, 0.20) * 2.6, band);
        alpha = max(alpha, band);
    }

    // --- Selection ring (blue, HDR, gentle breathe) — on top of everything ---
    if (params.x > 0.5) {
        let pulse = 0.85 + 0.15 * sin(t * 3.0);
        col = mix(col, vec3<f32>(0.40, 0.68, 1.0) * (3.4 * pulse), band);
        alpha = max(alpha, band);
    }

    // Focus dimming: recede non-focused-ring cards by scaling color only. The
    // material is alpha-masked (Mask(0.5)), so scaling alpha would clip the body
    // below the cutoff and vanish the card instead of fading it. Live action
    // (`live`) pierces the dim by design — see above.
    return vec4<f32>(col * dim.x + live, alpha);
}
