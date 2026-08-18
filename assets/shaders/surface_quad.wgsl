// Conversation-Surface Textured Quad Shader
//
// One instance per rasterized SVG. The CPU raster (text/svg_raster.rs) is the
// same one the legacy path hangs on a child `ImageNode`; here it is sampled
// onto a quad at the block's document rect, so it scrolls with the surface
// instead of with a UI node. See view/surface/rich.rs for the raster cache and
// its staleness rule.
//
// One bind group per quad — the texture differs per block and there are never
// many visible at once, so a per-quad bind group costs less than any scheme
// that would avoid it (an atlas, a texture array, sorting by texture).
//
// The raster is STRAIGHT alpha (resvg output, unpremultiplied) in an
// `Rgba8UnormSrgb` texture, so the sample arrives linear — which is what this
// target holds — and the fragment premultiplies for the One/OneMinusSrcAlpha
// blend every pass into this target uses.

struct Uniforms {
    viewport_phys: vec2<f32>,   // render target size, PHYSICAL px
    scroll_offset: f32,         // scroll position, LOGICAL document px
    scale: f32,                 // logical → physical (HiDPI factor)
    sdf_texel: vec2<f32>,
    msdf_range: f32,
    hint_amount: f32,
    stem_darkening: f32,
    horz_scale: f32,
    vert_scale: f32,
    text_bias: f32,
    gamma_correction: f32,
    time: f32,
    // Document (0,0) inside the render target, LOGICAL px.
    origin_logical: vec2<f32>,
}

@group(0) @binding(0) var<uniform> uniforms: Uniforms;
@group(0) @binding(1) var quad_texture: texture_2d<f32>;
@group(0) @binding(2) var quad_sampler: sampler;

struct InstanceInput {
    // [x, y, w, h] in document space, logical px.
    @location(0) rect_doc: vec4<f32>,
}

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) uv: vec2<f32>,
}

@vertex
fn vertex(@builtin(vertex_index) vertex_index: u32, inst: InstanceInput) -> VertexOutput {
    var corners = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0),
        vec2<f32>(1.0, 0.0),
        vec2<f32>(0.0, 1.0),
        vec2<f32>(1.0, 0.0),
        vec2<f32>(1.0, 1.0),
        vec2<f32>(0.0, 1.0),
    );
    let corner = corners[vertex_index];

    // X unsnapped; Y snapped at the rect's TOP edge and the rest placed
    // relative to it, so the image is rigid while it scrolls — the same rule
    // surface_chrome.wgsl applies to a border box.
    let x_phys = (inst.rect_doc.x + corner.x * inst.rect_doc.z + uniforms.origin_logical.x)
        * uniforms.scale;
    let y_top_phys = round(
        (inst.rect_doc.y - uniforms.scroll_offset + uniforms.origin_logical.y) * uniforms.scale
    );
    let y_phys = y_top_phys + corner.y * inst.rect_doc.w * uniforms.scale;

    var out: VertexOutput;
    out.clip_position = vec4<f32>(
        x_phys / uniforms.viewport_phys.x * 2.0 - 1.0,
        1.0 - y_phys / uniforms.viewport_phys.y * 2.0,
        0.0,
        1.0,
    );
    out.uv = corner;
    return out;
}

@fragment
fn fragment(in: VertexOutput) -> @location(0) vec4<f32> {
    let texel = textureSample(quad_texture, quad_sampler, in.uv);
    if texel.a < 0.004 {
        discard;
    }
    return vec4<f32>(texel.rgb * texel.a, texel.a);
}
