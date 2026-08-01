// Flat-Colored Music Geometry Shader
//
// Renders triangle-list geometry for ABC notation elements that aren't
// glyphs: staff lines, barlines, stems, ledgers, beams, slurs, ties, and
// repeat dots (see text/msdf/geometry.rs for the CPU-side triangulation).
// No texture, no per-fragment SDF — a plain premultiplied-alpha fill.

struct VertexInput {
    @location(0) position: vec2<f32>,  // NDC [-1, 1]
    @location(1) color: vec4<f32>,     // straight RGBA (unorm)
}

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) color: vec4<f32>,
}

@vertex
fn vertex(in: VertexInput) -> VertexOutput {
    var out: VertexOutput;
    out.clip_position = vec4<f32>(in.position, 0.0, 1.0);
    out.color = in.color;
    return out;
}

@fragment
fn fragment(in: VertexOutput) -> @location(0) vec4<f32> {
    // Premultiplied alpha output, matching msdf_block.wgsl's convention so
    // the two passes composite consistently into the same block texture.
    let premul_rgb = in.color.rgb * in.color.a;
    return vec4<f32>(premul_rgb, in.color.a);
}
