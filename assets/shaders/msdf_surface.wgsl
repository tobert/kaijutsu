// MSDF Conversation-Surface Text Shader (instanced)
//
// One instance per glyph, one draw per pane. Instances carry DOCUMENT-SPACE
// LOGICAL coordinates and never change while you scroll — the scroll offset
// is a uniform, so a wheel detent costs a 64-byte buffer write instead of a
// rebuilt vertex buffer. See text/msdf/surface_renderer.rs.
//
// ─── MIRROR CONTRACT ────────────────────────────────────────────────────────
// Three things in this file are mirrored in Rust and must move together:
//
//  1. `Uniforms` mirrors `SurfaceUniforms` (surface_renderer.rs) field for
//     field, including the two trailing pads that round it to 64 bytes.
//  2. The `InstanceInput` @location list mirrors `GlyphInstance` +
//     `ConversationSurfaceRenderer::instance_layout()`; the offsets are
//     pinned by `instance_layout_matches_the_struct`.
//  3. The baseline snap below is mirrored by `snap_baseline_phys()`, which
//     the surface tests use as the reference for every placement assertion.
//     Change the formula here and that function changes in the same commit.
//
// The fragment half is ported from msdf_block.wgsl (median / screen_px_range
// / hinting / stem darkening / gamma), deliberately verbatim so both surfaces
// shade text identically. The duplication is accepted for now and tracked as
// a follow-up (plan §Risks).
//
// A fourth mirrored item joined the contract on 2026-08-19: `StyleEntry`
// mirrors the Rust struct in surface_renderer.rs, and the style table at
// @binding(3) is how per-glyph styling stays dynamic without instance
// rebuilds — ANSI palette colors resolve here (retheme = table rewrite), and
// the reserved effect/param fields are the landing site for the rainbow/
// blink/CRT revival (docs/ansi-and-beyond.md).

struct Uniforms {
    viewport_phys: vec2<f32>,   // render target size, PHYSICAL px
    scroll_offset: f32,         // scroll position, LOGICAL document px
    scale: f32,                 // logical → physical (HiDPI factor)
    sdf_texel: vec2<f32>,       // (1/atlas_width, 1/atlas_height)
    msdf_range: f32,
    hint_amount: f32,
    stem_darkening: f32,
    horz_scale: f32,
    vert_scale: f32,
    text_bias: f32,
    gamma_correction: f32,
    time: f32,
    // Document (0,0) inside the render target, LOGICAL px. The texture covers
    // the pane's PADDING box while document coordinates are measured from its
    // CONTENT box; this is the difference. See view/surface/target.rs.
    origin_logical: vec2<f32>,
}

// One entry in the glyph style table — mirrors `StyleEntry` in
// surface_renderer.rs (32 bytes, vec4-aligned). Entry 0 is identity; entries
// 1..=256 are ANSI palette slots (index - 1), rewritten on retheme so themed
// terminal colors never require an instance rebuild. `effect`/`param` are
// reserved for the rainbow/blink/CRT revival.
struct StyleEntry {
    fg: vec4<f32>,
    flags: u32,     // bit 0: replace instance color with fg
    effect: u32,    // reserved, 0 = none
    param: f32,     // reserved
    _pad: f32,
}

@group(0) @binding(0) var<uniform> uniforms: Uniforms;
@group(0) @binding(1) var atlas_texture: texture_2d<f32>;
@group(0) @binding(2) var atlas_sampler: sampler;
@group(0) @binding(3) var<storage, read> style_table: array<StyleEntry>;

struct InstanceInput {
    // Pen position in DOCUMENT space, logical px: (x, baseline_y).
    @location(0) baseline_doc: vec2<f32>,
    // Pen → quad top-left, logical px (negated em-unit atlas anchor).
    @location(1) quad_offset: vec2<f32>,
    // Quad size, logical px (atlas region px * font_size / 64).
    @location(2) quad_size: vec2<f32>,
    // Atlas UV rect [u0, v0, u1, v1].
    @location(3) uv_rect: vec4<f32>,
    @location(4) color: vec4<f32>,
    @location(5) importance: f32,
    // Style table index; 0 = unstyled (color above stands).
    @location(6) style_index: u32,
}

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) color: vec4<f32>,
    @location(2) importance: f32,
}

// ============================================================================
// VERTEX SHADER
// ============================================================================

@vertex
fn vertex(@builtin(vertex_index) vertex_index: u32, inst: InstanceInput) -> VertexOutput {
    // Six vertices, non-indexed, in the same winding msdf_block's
    // build_vertices emits: TL, TR, BL / TR, BR, BL.
    var corners = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0),
        vec2<f32>(1.0, 0.0),
        vec2<f32>(0.0, 1.0),
        vec2<f32>(1.0, 0.0),
        vec2<f32>(1.0, 1.0),
        vec2<f32>(0.0, 1.0),
    );
    let corner = corners[vertex_index];

    // X: logical document space → physical, NEVER snapped. Sub-pixel
    // advances are what keep kerning and monospace columns correct.
    let x_phys = (inst.baseline_doc.x + inst.quad_offset.x + corner.x * inst.quad_size.x
        + uniforms.origin_logical.x) * uniforms.scale;

    // Y: subtract the scroll offset in LOGICAL space, convert to physical,
    // and snap the BASELINE (not the corner) to an integer physical row.
    // Mirror of snap_baseline_phys() in surface_renderer.rs — the document
    // origin is folded into the doc-space Y before the identical formula, so
    // the snap still lands on a real physical scanline.
    let y_base_phys = round(
        (inst.baseline_doc.y - uniforms.scroll_offset + uniforms.origin_logical.y) * uniforms.scale
    );
    // The quad is then placed relative to the snapped baseline, so a glyph's
    // shape is rigid — only where the whole quad lands moves with scroll.
    let y_phys = y_base_phys + (inst.quad_offset.y + corner.y * inst.quad_size.y) * uniforms.scale;

    var out: VertexOutput;
    out.clip_position = vec4<f32>(
        x_phys / uniforms.viewport_phys.x * 2.0 - 1.0,
        1.0 - y_phys / uniforms.viewport_phys.y * 2.0,
        0.0,
        1.0,
    );
    // V flipped: msdfgen bitmaps are Y-up, GPU textures are Y-down, so the
    // quad's top corner samples v1 (uv_rect.w) and the bottom v0 (uv_rect.y).
    out.uv = vec2<f32>(
        mix(inst.uv_rect.x, inst.uv_rect.z, corner.x),
        mix(inst.uv_rect.w, inst.uv_rect.y, corner.y),
    );
    // Resolve the style table: a themed entry replaces the instance color
    // (index 0 is the identity entry, flags == 0). The alpha channel stays
    // the instance's — spans style color, not visibility.
    let style = style_table[min(inst.style_index, arrayLength(&style_table) - 1u)];
    let use_fg = (style.flags & 1u) != 0u;
    out.color = vec4<f32>(
        select(inst.color.rgb, style.fg.rgb, use_fg),
        inst.color.a,
    );
    out.importance = inst.importance;
    return out;
}

// ============================================================================
// MSDF UTILITIES (ported verbatim from msdf_block.wgsl)
// ============================================================================

/// Compute the median of three values — core of MSDF distance extraction.
fn median(r: f32, g: f32, b: f32) -> f32 {
    return max(min(r, g), min(max(r, g), b));
}

/// Compute screen-space pixel range for adaptive anti-aliasing.
fn screen_px_range(uv: vec2<f32>) -> f32 {
    let atlas_size = 1.0 / uniforms.sdf_texel;
    let uv_fwidth = fwidth(uv);
    let pixel_dist = max(uv_fwidth.x, uv_fwidth.y);
    let unit_range = uniforms.msdf_range / atlas_size.x;
    return max(unit_range / pixel_dist, 1.0);
}

/// Shader-based hinting with stem darkening and semantic weighting.
fn msdf_alpha_hinted(uv: vec2<f32>, bias: f32, importance: f32) -> f32 {
    let sample_c = textureSample(atlas_texture, atlas_sampler, uv);
    let sample_n = textureSample(atlas_texture, atlas_sampler, uv + vec2<f32>(0.0, uniforms.sdf_texel.y));
    let sample_e = textureSample(atlas_texture, atlas_sampler, uv + vec2<f32>(uniforms.sdf_texel.x, 0.0));

    let sd_c = min(median(sample_c.r, sample_c.g, sample_c.b), sample_c.a);
    let sd_n = min(median(sample_n.r, sample_n.g, sample_n.b), sample_n.a);
    let sd_e = min(median(sample_e.r, sample_e.g, sample_e.b), sample_e.a);

    let sgrad = vec2<f32>(sd_e - sd_c, sd_n - sd_c);
    let sgrad_len = max(length(sgrad), 1.0 / 128.0);
    let grad = sgrad / sgrad_len;

    // vgrad: 0 = vertical stroke edge, 1 = horizontal stroke edge
    let vgrad = abs(grad.y);

    let px_range = screen_px_range(uv);
    let base_doffset = 0.5 / (px_range * 2.0);

    let darkening = uniforms.stem_darkening * clamp(1.0 / px_range, 0.0, 0.5);
    let weight_adjust = mix(0.02, -0.015, importance);
    let effective_bias = bias - darkening + weight_adjust;

    let hinted_doffset = mix(
        base_doffset * uniforms.horz_scale,
        base_doffset * uniforms.vert_scale,
        vgrad
    );
    let doffset = mix(base_doffset, hinted_doffset, uniforms.hint_amount);

    var alpha = smoothstep(effective_bias - doffset, effective_bias + doffset, sd_c);
    alpha = pow(alpha, 1.0 + 0.2 * vgrad * uniforms.hint_amount);
    alpha = pow(alpha, uniforms.gamma_correction);

    return alpha;
}

// ============================================================================
// FRAGMENT SHADER
// ============================================================================

@fragment
fn fragment(in: VertexOutput) -> @location(0) vec4<f32> {
    let text_alpha = msdf_alpha_hinted(in.uv, uniforms.text_bias, in.importance);

    // Premultiplied alpha output — the pipeline blends One / OneMinusSrcAlpha
    // over the surface's opaque background clear.
    let final_alpha = text_alpha * in.color.a;
    let premul_rgb = in.color.rgb * final_alpha;

    if final_alpha < 0.004 {
        discard;
    }

    return vec4<f32>(premul_rgb, final_alpha);
}
