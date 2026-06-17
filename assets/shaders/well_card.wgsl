// Well Card Shader — 3D material for time-well cards (rim + focus).
//
// Draws the whole card on the GPU (vello-free): an accent rounded-rect body
// (SDF), selection/lineage rings (SDF, from `params`), and the MSDF text
// composited on top (the `card_texture`, rendered text-on-transparent by the
// MSDF pass). Masked alpha → the rounded corners discard cleanly.

#import bevy_pbr::forward_io::VertexOutput

@group(#{MATERIAL_BIND_GROUP}) @binding(0) var card_texture: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(1) var card_sampler: sampler;
@group(#{MATERIAL_BIND_GROUP}) @binding(2) var<uniform> accent: vec4<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(3) var<uniform> params: vec4<f32>; // [selected, in_lineage, status, time]
@group(#{MATERIAL_BIND_GROUP}) @binding(4) var<uniform> shape: vec4<f32>;  // [aspect, corner_radius, ring_width, inset]

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

    // Accent body.
    var col = accent.rgb;
    var alpha = accent.a * inside;

    // MSDF text on top (text texture is transparent except glyphs).
    let text = textureSample(card_texture, card_sampler, in.uv);
    col = mix(col, text.rgb, text.a);
    alpha = max(alpha, text.a * inside);

    // Ring band hugging the inner edge of the rounded box.
    let band = (1.0 - smoothstep(ring_w, ring_w + aa, abs(d))) * inside;
    if (params.y > 0.5) { // lineage (amber)
        col = mix(col, vec3<f32>(0.95, 0.70, 0.20), band);
        alpha = max(alpha, band);
    }
    if (params.x > 0.5) { // selection (blue) — over lineage
        col = mix(col, vec3<f32>(0.40, 0.68, 1.0), band);
        alpha = max(alpha, band);
    }

    return vec4<f32>(col, alpha);
}
