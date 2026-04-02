// Stack Card Shader — 3D material for conversation stack cards.

#import bevy_pbr::forward_io::VertexOutput

@group(#{MATERIAL_BIND_GROUP}) @binding(0) var card_texture: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(1) var card_sampler: sampler;

struct StackCardUniforms {
    card_params: vec4<f32>,
    glow_color: vec4<f32>,
    glow_params: vec4<f32>,
};

@group(#{MATERIAL_BIND_GROUP}) @binding(2) var<uniform> uniforms: StackCardUniforms;

@fragment
fn fragment(in: VertexOutput) -> @location(0) vec4<f32> {
    let texture_color = textureSample(card_texture, card_sampler, in.uv);
    let opacity = uniforms.card_params.x;
    let glow_color = uniforms.glow_color;
    let glow_intensity = uniforms.glow_params.x;

    // Simple edge glow: boost role color at UV boundaries
    let edge_dist = min(min(in.uv.x, 1.0 - in.uv.x), min(in.uv.y, 1.0 - in.uv.y));
    let edge_glow = smoothstep(0.0, 0.05, 0.05 - edge_dist) * glow_intensity;
    
    let final_rgb = mix(texture_color.rgb, glow_color.rgb, edge_glow);
    let final_a = max(texture_color.a, edge_glow * glow_color.a) * opacity;
    
    return vec4<f32>(final_rgb, final_a);
}
