// Conversation-Surface Geometry Shader (flat-colored triangles)
//
// Staff lines, beams and slurs for ABC notation; per-line background bands and
// word washes for diffs; sparkline plots; the image placeholder rect. All of
// them arrive as a plain triangle list in DOCUMENT-SPACE LOGICAL coordinates
// and never change while you scroll — same contract as msdf_surface.wgsl,
// same uniforms, same pass. See view/surface/rich.rs for the builders.
//
// ─── PORT NOTE ──────────────────────────────────────────────────────────────
// The fragment half is music_geometry.wgsl verbatim (premultiplied-alpha
// fill). What changed is the vertex half: music_geometry.wgsl receives NDC
// computed on the CPU per block texture, because on the legacy path every
// block owns a texture whose size is known at build time. Here the vertices
// outlive the scroll position, so the doc→physical transform moved into the
// shader — exactly the move msdf_surface.wgsl made for glyphs.
//
// ─── WHY THE ANCHOR ─────────────────────────────────────────────────────────
// Snapping every vertex to a physical row independently would deform the
// shape: a 1.2px staff line whose two edges round the same way vanishes, and
// whose edges round apart doubles. So each run carries ONE anchor — the
// chunk's document y — which is snapped exactly like a glyph baseline
// (snap_baseline_phys in surface_renderer.rs), and every vertex is placed at
// an unsnapped offset from it. The shape stays rigid; only where it lands
// moves with the scroll. This is the same rule surface_chrome.wgsl applies to
// its box top.

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

struct VertexInput {
    // x: document x, LOGICAL px. y: offset from the run's anchor, logical px.
    @location(0) pos_doc: vec2<f32>,
    // The run's document y — snapped as a unit, see above.
    @location(1) anchor_doc_y: f32,
    // Straight RGBA (unorm); premultiplied in the fragment shader.
    @location(2) color: vec4<f32>,
}

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) color: vec4<f32>,
}

@vertex
fn vertex(in: VertexInput) -> VertexOutput {
    // X: logical → physical, NEVER snapped (same rule as the glyph pass).
    let x_phys = (in.pos_doc.x + uniforms.origin_logical.x) * uniforms.scale;

    let y_anchor_phys = round(
        (in.anchor_doc_y - uniforms.scroll_offset + uniforms.origin_logical.y) * uniforms.scale
    );
    let y_phys = y_anchor_phys + in.pos_doc.y * uniforms.scale;

    var out: VertexOutput;
    out.clip_position = vec4<f32>(
        x_phys / uniforms.viewport_phys.x * 2.0 - 1.0,
        1.0 - y_phys / uniforms.viewport_phys.y * 2.0,
        0.0,
        1.0,
    );
    out.color = in.color;
    return out;
}

@fragment
fn fragment(in: VertexOutput) -> @location(0) vec4<f32> {
    // Premultiplied alpha, matching every other pass into this target.
    let premul_rgb = in.color.rgb * in.color.a;
    return vec4<f32>(premul_rgb, in.color.a);
}
