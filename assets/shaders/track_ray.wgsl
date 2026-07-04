// Track Ray Shader — a track's beam down the funnel wall of the time well.
//
// The quad's local +X runs THROAT → MOUTH (uv.x: 0 = throat end, 1 = mouth
// end); uv.y crosses the beam's width. The beam is a soft filament in the
// track's hue; while the transport plays, a bright pulse rides it from the
// mouth (uv.x = 1) down into the throat (uv.x = 0) each beat — energy falling
// into the well. Bright values go HDR (>1.0) so the shared bloom pass glows
// them (bright = live action; a stopped ray stays LDR).
//
// color  = [r, g, b, base_alpha] — track hue
// params = [beat_env, playing, beat_frac, activity]

#import bevy_pbr::forward_io::VertexOutput

@group(#{MATERIAL_BIND_GROUP}) @binding(0) var<uniform> color: vec4<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(1) var<uniform> params: vec4<f32>;

@fragment
fn fragment(in: VertexOutput) -> @location(0) vec4<f32> {
    let beat_env = params.x;
    let playing = params.y;
    let beat_frac = params.z;
    let activity = params.w;

    // Soft filament across the width: bright core line, gaussian falloff.
    let across = abs(in.uv.y - 0.5) * 2.0;            // 0 at core → 1 at edge
    let core = exp(-across * across * 9.0);

    // Fade the beam's ends: melt into the throat glow, taper at the mouth.
    let ends = smoothstep(0.0, 0.06, in.uv.x) * (1.0 - smoothstep(0.94, 1.0, in.uv.x));

    // Base filament: dim structure when stopped, lifted while playing and by
    // attached-context activity (chatter travels up the lane).
    let base = (0.22 + playing * 0.30 + activity * 0.55) * core * ends;
    var col = color.rgb * base;
    var alpha = color.a * base;

    // The beat pulse: a bright packet at `1 - beat_frac` along the beam (beat
    // onset at the mouth, sliding into the throat through the beat), gated by
    // the transport and scaled by the envelope. HDR so it blooms.
    if (playing > 0.5) {
        let pos = 1.0 - beat_frac;
        let d = (in.uv.x - pos);
        let packet = exp(-d * d * 380.0) * core * ends;
        let hot = packet * (0.5 + beat_env * 2.6);
        col += color.rgb * hot * 2.4;
        alpha = max(alpha, min(hot, 1.0));
    }

    return vec4<f32>(col, alpha);
}
