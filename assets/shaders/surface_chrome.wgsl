// Conversation-Surface Chrome Shader (instanced SDF quads)
//
// One instance per border box: block borders, the focus ring, and the
// role-group divider rule. Instances carry DOCUMENT-SPACE LOGICAL rects and
// never change while you scroll — same contract as msdf_surface.wgsl, same
// uniforms, same pass. See view/surface/chrome.rs.
//
// ─── PORT NOTE ──────────────────────────────────────────────────────────────
// The SDF math below is ported from assets/shaders/block_fx.wgsl, which draws
// the same chrome on the legacy path as a per-block UiMaterial. Ported
// verbatim: sd_rounded_box, perimeter_param, border_stroke_alpha (every
// BK_* kind), the breathe/pulse/chase animation modes, and the edge glow.
//
// Also ported: the fieldset label insets (border_stroke.zw on the legacy
// side) and the TOP/BOTTOM label gaps, so a block's captions straddle their
// own stroke. The surface shapes those glyphs in view/surface/labels.rs and
// draws them in the glyph pass; this shader only cuts the hole.
//
// NOT ported, because the surface path has no such content (yet):
//   * the block texture composite, the 9-tap text halo, and the gutter
//     indicator flag — those all read a per-block texture that does not exist
//     here (glyphs are drawn as instances into the same target instead);
//   * the cursor beam and the selection wash — compose/editor surfaces, not
//     the conversation;
//   * the chase-through-label brightening. block_fx can boost a label glyph
//     because the glyph is IN the texture it is shading; here the glyphs are a
//     later pass this one cannot reach. See docs/issues.md.
//
// Because block_fx composites its border and glow BEHIND the text (both are
// masked by `1 - result.a`), the surface draws this pass FIRST and the glyph
// pass over it. That is the whole draw order.

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
    // Document origin inside the render target, LOGICAL px: the pane's
    // padding box is what this texture covers, and document x/y = 0 is the
    // CONTENT box's top-left. See view/surface/target.rs.
    origin_logical: vec2<f32>,
}

@group(0) @binding(0) var<uniform> uniforms: Uniforms;

struct InstanceInput {
    // Border box [x, y, w, h] in document space, logical px.
    @location(0) rect_doc: vec4<f32>,
    // [corner_radius, thickness, glow_radius, glow_intensity]
    @location(1) params: vec4<f32>,
    // Stroke-suppressed gaps, rect-local logical px:
    // [top_x0, top_x1, bottom_x0, bottom_x1]. All zero = none. CENTER_LINE
    // uses only .xy. Same packing as block_fx.wgsl's label_gaps uniform.
    @location(2) label_gap: vec4<f32>,
    // Fieldset insets [top, bottom]: how far the stroke moves inward so a
    // label can straddle it. 0 = the default 1px AA inset.
    @location(3) insets: vec2<f32>,
    // [anim_mode, phase]
    @location(4) anim: vec2<f32>,
    @location(5) color: vec4<f32>,
    @location(6) kind: u32,
}

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    // Position within the rect, logical px, centered on it (block_fx's `p`).
    @location(0) local_px: vec2<f32>,
    @location(1) half_size: vec2<f32>,
    @location(2) color: vec4<f32>,
    @location(3) params: vec4<f32>,
    @location(4) label_gap: vec4<f32>,
    @location(5) anim: vec2<f32>,
    @location(6) @interpolate(flat) kind: u32,
    @location(7) insets: vec2<f32>,
}

// Border kinds — the same numbering as block_fx.wgsl's BK_* and
// chrome::CHROME_KIND_*.
const BK_NONE: u32 = 0u;
const BK_FULL: u32 = 1u;
const BK_TOP_ACCENT: u32 = 2u;
const BK_DASHED: u32 = 3u;
const BK_OPEN_BOTTOM: u32 = 4u;
const BK_OPEN_TOP: u32 = 5u;
const BK_CENTER_LINE: u32 = 6u;

// ============================================================================
// VERTEX SHADER
// ============================================================================

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

    let half_size = inst.rect_doc.zw * 0.5;
    // The stroke straddles the border rect (abs(d) < thickness/2) and the
    // dash/chase masks read a pixel either side, so the drawn quad is grown a
    // little past the rect. The glow decays INWARD (block_fx clips it to the
    // node), so nothing else needs room.
    let pad = max(inst.params.y, 1.0) + 1.0;
    let local = (corner * 2.0 - 1.0) * (half_size + vec2<f32>(pad, pad));

    let center_doc = inst.rect_doc.xy + half_size;

    // X: logical → physical, never snapped (same rule as the glyph pass).
    let x_phys = (center_doc.x + local.x + uniforms.origin_logical.x) * uniforms.scale;

    // Y: the rect's TOP edge is snapped to a physical row — a 1px divider or
    // a 1.5px stroke shimmers otherwise as the offset eases. Mirror of
    // snap_baseline_phys() in surface_renderer.rs, applied to the box origin
    // instead of a baseline; everything inside is placed relative to it, so
    // the box is rigid.
    let y_top_phys = round(
        (inst.rect_doc.y - uniforms.scroll_offset + uniforms.origin_logical.y) * uniforms.scale
    );
    let y_phys = y_top_phys + (local.y + half_size.y) * uniforms.scale;

    var out: VertexOutput;
    out.clip_position = vec4<f32>(
        x_phys / uniforms.viewport_phys.x * 2.0 - 1.0,
        1.0 - y_phys / uniforms.viewport_phys.y * 2.0,
        0.0,
        1.0,
    );
    out.local_px = local;
    out.half_size = half_size;
    out.color = inst.color;
    out.params = inst.params;
    out.label_gap = inst.label_gap;
    out.insets = inst.insets;
    out.anim = inst.anim;
    out.kind = inst.kind;
    return out;
}

// ============================================================================
// SDF UTILITIES (ported verbatim from block_fx.wgsl)
// ============================================================================

// Rounded box SDF: negative inside, zero on edge, positive outside.
fn sd_rounded_box(p: vec2<f32>, b: vec2<f32>, r: f32) -> f32 {
    let q = abs(p) - b + r;
    return min(max(q.x, q.y), 0.0) + length(max(q, vec2<f32>(0.0))) - r;
}

// Perimeter parameterization: maps a point to 0..1 around the border perimeter.
// Traversal order: top (left→right) → right (top→bottom) → bottom (right→left) → left (bottom→top).
fn perimeter_param(p: vec2<f32>, half: vec2<f32>) -> f32 {
    let perim = 2.0 * (half.x + half.y);
    if perim <= 0.0 { return 0.0; }
    let dx_left = p.x + half.x;
    let dx_right = half.x - p.x;
    let dy_top = p.y + half.y;
    let dy_bottom = half.y - p.y;
    let min_d = min(min(dx_left, dx_right), min(dy_top, dy_bottom));

    var t = 0.0;
    if dy_top <= min_d + 0.5 {
        t = (p.x + half.x) / perim;
    } else if dx_right <= min_d + 0.5 {
        t = (half.x * 2.0 + p.y + half.y) / perim;
    } else if dy_bottom <= min_d + 0.5 {
        t = (half.x * 2.0 + half.y * 2.0 + half.x - p.x) / perim;
    } else {
        t = 1.0 - (p.y + half.y) / perim;
    }
    return clamp(t, 0.0, 1.0);
}

// Border stroke alpha at the box edge. `p` is centered on the rect.
//
// `insets` is block_fx's border_stroke.zw: the fieldset straddle, which moves
// the top and/or bottom stroke inward by half a label's ascent so the letters
// sit ON the line. Zero on either side means the default 1px AA inset. The box
// becomes ASYMMETRIC when only one edge is inset, so the SDF is evaluated
// against a re-centered point (`bp`) — same construction as block_fx.
fn border_stroke_alpha(
    p: vec2<f32>,
    half_size: vec2<f32>,
    insets: vec2<f32>,
    thickness: f32,
    corner_r: f32,
    kind: u32,
) -> f32 {
    let inset_top = select(1.0, insets.x, insets.x > 0.0);
    let inset_bottom = select(1.0, insets.y, insets.y > 0.0);
    let center_y = (inset_top - inset_bottom) * 0.5;
    let border_half = vec2<f32>(
        half_size.x - 1.0,
        half_size.y - (inset_top + inset_bottom) * 0.5,
    );
    let aa = 1.0;
    let bp = vec2<f32>(p.x, p.y - center_y);

    if kind == BK_TOP_ACCENT {
        // Just the top edge: horizontal line near the top of the box.
        let line_y = -border_half.y;
        let line_x0 = -border_half.x;
        let line_x1 = border_half.x;
        let dy = abs(bp.y - line_y);
        let in_x = smoothstep(line_x0 - aa, line_x0, bp.x) * (1.0 - smoothstep(line_x1, line_x1 + aa, bp.x));
        return (1.0 - smoothstep(thickness * 0.5, thickness * 0.5 + aa, dy)) * in_x;
    }

    if kind == BK_CENTER_LINE {
        // Role-group divider: one full-width rule through the vertical
        // center. No box, no insets.
        let dy = abs(p.y);
        let in_x = step(abs(p.x), half_size.x);
        return (1.0 - smoothstep(thickness * 0.5, thickness * 0.5 + aa, dy)) * in_x;
    }

    let d = sd_rounded_box(bp, border_half, corner_r);
    var alpha = 1.0 - smoothstep(0.0, aa, abs(d) - thickness * 0.5);

    if kind == BK_OPEN_BOTTOM {
        // Suppress bottom edge + bottom corners; side edges run to the box bottom.
        let bottom_y = border_half.y - corner_r;
        let bottom_mask = smoothstep(bottom_y, bottom_y + corner_r, bp.y);
        let near_left = abs(bp.x - (-border_half.x)) < thickness;
        let near_right = abs(bp.x - border_half.x) < thickness;
        let is_side_edge = select(0.0, 1.0, near_left || near_right);
        if is_side_edge > 0.5 && bp.y > bottom_y {
            let side_x = select(border_half.x, -border_half.x, near_left);
            let side_d = abs(bp.x - side_x);
            alpha = 1.0 - smoothstep(0.0, aa, side_d - thickness * 0.5);
        } else {
            alpha *= (1.0 - bottom_mask);
        }
    } else if kind == BK_OPEN_TOP {
        // Suppress top edge + top corners; side edges run to the box top.
        let top_y = -border_half.y + corner_r;
        let top_mask = smoothstep(top_y, top_y - corner_r, bp.y);
        let near_left = abs(bp.x - (-border_half.x)) < thickness;
        let near_right = abs(bp.x - border_half.x) < thickness;
        let is_side_edge = select(0.0, 1.0, near_left || near_right);
        if is_side_edge > 0.5 && bp.y < top_y {
            let side_x = select(border_half.x, -border_half.x, near_left);
            let side_d = abs(bp.x - side_x);
            alpha = 1.0 - smoothstep(0.0, aa, side_d - thickness * 0.5);
        } else {
            alpha *= (1.0 - top_mask);
        }
        // Horizontal divider line at the top of this block (joins the call above).
        let divider_y = -border_half.y;
        let div_d = abs(bp.y - divider_y);
        let div_x0 = -border_half.x;
        let div_x1 = border_half.x;
        let in_x = smoothstep(div_x0 - aa, div_x0, bp.x) * (1.0 - smoothstep(div_x1, div_x1 + aa, bp.x));
        let div_alpha = (1.0 - smoothstep(thickness * 0.5, thickness * 0.5 + aa, div_d)) * in_x;
        alpha = max(alpha, div_alpha);
    } else if kind == BK_DASHED {
        let perim = 2.0 * (border_half.x + border_half.y);
        var t = 0.0;
        if bp.y <= -border_half.y + aa {
            t = (bp.x + border_half.x) / perim;
        } else if bp.x >= border_half.x - aa {
            t = (border_half.x * 2.0 + bp.y + border_half.y) / perim;
        } else if bp.y >= border_half.y - aa {
            t = (border_half.x * 2.0 + border_half.y * 2.0 + border_half.x - bp.x) / perim;
        } else {
            t = 1.0 - (bp.y + border_half.y) / perim;
        }
        let dash_count = 40.0;
        let dash_duty = 0.6;
        let dash_pattern = smoothstep(dash_duty - 0.02, dash_duty + 0.02, fract(t * dash_count));
        alpha *= (1.0 - dash_pattern);
    }

    return alpha;
}

// ============================================================================
// FRAGMENT SHADER
// ============================================================================

@fragment
fn fragment(in: VertexOutput) -> @location(0) vec4<f32> {
    if in.kind == BK_NONE {
        discard;
    }

    let corner_r = in.params.x;
    let thickness = in.params.y;
    let glow_radius = in.params.z;
    let glow_intensity = in.params.w;
    let anim_mode = in.anim.x;
    let t = uniforms.time + in.anim.y;

    let p = in.local_px;
    let half_size = in.half_size;

    // --- Animation multiplier (shared by stroke and glow) ---
    var anim = 1.0;
    if anim_mode == 1.0 {
        anim = 0.7 + 0.3 * sin(t);
    } else if anim_mode == 2.0 {
        anim = 0.4 + 0.6 * sin(t * 3.0);
    } else if anim_mode == 3.0 {
        // Chase: perimeter-based traveling light.
        let param = perimeter_param(p, half_size);
        let chase_speed = 0.4;
        let chase_width = 0.15;
        let wave = fract(param - t * chase_speed);
        anim = smoothstep(chase_width, 0.0, wave) + 0.15;
    }

    // --- Border stroke ---
    var stroke_a = border_stroke_alpha(p, half_size, in.insets, thickness, corner_r, in.kind) * anim;

    // Label gaps: the stroke stops where a caption sits. Ported from
    // block_fx.wgsl's gap block, including the near_top/near_bottom bands that
    // keep a top caption from punching a hole in the bottom edge underneath it.
    let px_x = p.x + half_size.x;
    if in.kind == BK_CENTER_LINE {
        // A rule through the vertical center has no top/bottom distinction —
        // the line itself already limits y, so mask on x alone.
        if (in.label_gap.x > 0.0 || in.label_gap.y > 0.0)
            && px_x >= in.label_gap.x && px_x <= in.label_gap.y {
            stroke_a = 0.0;
        }
    } else {
        let eff_inset_top = select(1.0, in.insets.x, in.insets.x > 0.0);
        let eff_inset_bottom = select(1.0, in.insets.y, in.insets.y > 0.0);
        if in.label_gap.x > 0.0 || in.label_gap.y > 0.0 {
            let near_top = p.y < -half_size.y + eff_inset_top + thickness;
            if near_top && px_x >= in.label_gap.x && px_x <= in.label_gap.y {
                stroke_a = 0.0;
            }
        }
        if in.label_gap.z > 0.0 || in.label_gap.w > 0.0 {
            let near_bottom = p.y > half_size.y - eff_inset_bottom - thickness;
            if near_bottom && px_x >= in.label_gap.z && px_x <= in.label_gap.w {
                stroke_a = 0.0;
            }
        }
    }

    var rgb = vec3<f32>(0.0);
    var a = 0.0;
    if stroke_a > 0.0 {
        let ba = in.color.a * stroke_a;
        rgb = in.color.rgb * ba;
        a = ba;
    }

    // --- Edge glow ---
    // block_fx clamps this to the node by construction (the material only
    // shades the node's own quad); the quad here is a little larger, so the
    // distance is clamped instead — outside the box, exp(d/r) would grow
    // rather than decay.
    if glow_radius > 0.0 {
        let d = sd_rounded_box(p, half_size, corner_r);
        let edge_glow = exp(min(d, 0.0) / glow_radius) * glow_intensity * anim;
        let behind = 1.0 - a;
        rgb = rgb + in.color.rgb * edge_glow * behind;
        a = max(a, edge_glow * in.color.a * behind);
    }

    if a < 0.004 {
        discard;
    }
    // Premultiplied — the pipeline blends One / OneMinusSrcAlpha, same as the
    // glyph pass.
    return vec4<f32>(rgb, a);
}
