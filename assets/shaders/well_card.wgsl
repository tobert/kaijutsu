// Well Card Shader — 3D material for time-well cards (rim + focus).
//
// Slice 1: sample the card's RTT content texture unchanged (parity with the old
// unlit StandardMaterial). The `uniforms` binding is declared now so slice 2 can
// add in-shader SDF glow / selection-rim / status FX without a layout change —
// the same texture=content / shader=FX split as block_fx.wgsl, on a 3D quad.

#import bevy_pbr::forward_io::VertexOutput

@group(#{MATERIAL_BIND_GROUP}) @binding(0) var card_texture: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(1) var card_sampler: sampler;

struct WellCardUniforms {
    // [selected, in_lineage, status, time] — reserved for slice-2 FX.
    params: vec4<f32>,
};
@group(#{MATERIAL_BIND_GROUP}) @binding(2) var<uniform> uniforms: WellCardUniforms;

@fragment
fn fragment(in: VertexOutput) -> @location(0) vec4<f32> {
    let c = textureSample(card_texture, card_sampler, in.uv);
    // `* 0.0` keeps the uniform referenced (so the binding survives) while
    // leaving slice-1 output identical to a plain texture sample.
    return c + uniforms.params * 0.0;
}
