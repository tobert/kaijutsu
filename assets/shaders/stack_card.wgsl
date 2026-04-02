// Stack Card Shader — 3D material for conversation stack cards.

#import bevy_pbr::forward_io::VertexOutput

@group(2) @binding(0) var card_texture: texture_2d<f32>;
@group(2) @binding(1) var card_sampler: sampler;

struct StackCardUniforms {
    card_params: vec4<f32>,
    glow_color: vec4<f32>,
    glow_params: vec4<f32>,
};

@group(2) @binding(2) var<uniform> uniforms: StackCardUniforms;

@fragment
fn fragment(in: VertexOutput) -> @location(0) vec4<f32> {
    let color = textureSample(card_texture, card_sampler, in.uv);
    let opacity = uniforms.card_params.x;
    return vec4<f32>(color.rgb * opacity, color.a * opacity);
}
