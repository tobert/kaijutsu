// Block FX Shader — post-process layer on MSDF-rendered block textures.
//
// MSDF renders text to per-block textures. This shader composites the
// texture and adds GPU-native effects: SDF border stroke + glow, animation
// overlays, text halo, and cursor beam.
//
// Uniforms:
//   glow_color       - Color for the border glow effect (RGBA linear)
//   fx_params        - [glow_radius, glow_intensity, animation_mode, corner_radius]
//     animation_mode: 0=none, 1=breathe, 2=pulse, 3=chase
//   text_glow_color  - Color for text halo (RGBA linear)
//   text_glow_params - [radius_px, 0, 0, 0]  (radius=0 disables)
//   cursor_params    - [x_uv, y_uv, width_uv, height_uv] (all 0 = disabled)
//   cursor_color     - RGBA color for cursor beam (linear)
//   border_stroke    - [thickness_px, border_kind, label_inset_top, label_inset_bottom]
//     border_kind: 0=none, 1=full, 2=top_accent, 3=dashed, 4=open_bottom, 5=open_top
//     label_inset_top/bottom: >0 moves border inward for fieldset/legend labels; 0=default (1px)
//   border_insets    - [pad_top, pad_bottom, pad_left, pad_right] in pixels
//   border_color     - RGBA color for border stroke (linear)

#import bevy_ui::ui_vertex_output::UiVertexOutput
#import bevy_render::globals::Globals

@group(0) @binding(1) var<uniform> globals: Globals;

@group(1) @binding(0) var block_texture: texture_2d<f32>;
@group(1) @binding(1) var block_sampler: sampler;
@group(1) @binding(2) var<uniform> glow_color: vec4<f32>;
@group(1) @binding(3) var<uniform> fx_params: vec4<f32>;
@group(1) @binding(4) var<uniform> text_glow_color: vec4<f32>;
@group(1) @binding(5) var<uniform> text_glow_params: vec4<f32>;
@group(1) @binding(6) var<uniform> cursor_params: vec4<f32>;
@group(1) @binding(7) var<uniform> cursor_color: vec4<f32>;
@group(1) @binding(8) var<uniform> border_stroke: vec4<f32>;
@group(1) @binding(9) var<uniform> border_insets: vec4<f32>;
@group(1) @binding(10) var<uniform> border_color: vec4<f32>;
@group(1) @binding(11) var<uniform> label_gaps: vec4<f32>;

// Border kind constants
const BK_NONE: f32 = 0.0;
const BK_FULL: f32 = 1.0;
const BK_TOP_ACCENT: f32 = 2.0;
const BK_DASHED: f32 = 3.0;
const BK_OPEN_BOTTOM: f32 = 4.0;
const BK_OPEN_TOP: f32 = 5.0;

// Rounded box SDF: negative inside, zero on edge, positive outside.
fn sd_rounded_box(p: vec2<f32>, b: vec2<f32>, r: f32) -> f32 {
    let q = abs(p) - b + r;
    return min(max(q.x, q.y), 0.0) + length(max(q, vec2<f32>(0.0))) - r;
}

// 9-tap text glow: samples 8 neighbors around each pixel to detect nearby
// text alpha, then blends a colored halo behind transparent areas.
fn text_glow_alpha(uv: vec2<f32>, radius_px: f32) -> f32 {
    let dims = vec2<f32>(textureDimensions(block_texture, 0));
    let step = radius_px / dims;

    let acc =
        textureSampleLevel(block_texture, block_sampler, uv + vec2<f32>( 0.0, -step.y), 0.0).a * 2.0 +
        textureSampleLevel(block_texture, block_sampler, uv + vec2<f32>( 0.0,  step.y), 0.0).a * 2.0 +
        textureSampleLevel(block_texture, block_sampler, uv + vec2<f32>(-step.x,  0.0), 0.0).a * 2.0 +
        textureSampleLevel(block_texture, block_sampler, uv + vec2<f32>( step.x,  0.0), 0.0).a * 2.0 +
        textureSampleLevel(block_texture, block_sampler, uv + vec2<f32>(-step.x, -step.y), 0.0).a +
        textureSampleLevel(block_texture, block_sampler, uv + vec2<f32>( step.x, -step.y), 0.0).a +
        textureSampleLevel(block_texture, block_sampler, uv + vec2<f32>(-step.x,  step.y), 0.0).a +
        textureSampleLevel(block_texture, block_sampler, uv + vec2<f32>( step.x,  step.y), 0.0).a;

    return acc / 12.0;
}

// Perimeter parameterization: maps a point to 0..1 around the border perimeter.
// Traversal order: top (left→right) → right (top→bottom) → bottom (right→left) → left (bottom→top).
// Uses the nearest edge to the point, giving constant-speed chase travel along rect edges.
fn perimeter_param(p: vec2<f32>, half: vec2<f32>) -> f32 {
    let perim = 2.0 * (half.x + half.y);
    if perim <= 0.0 { return 0.0; }
    // Determine which edge the point is nearest to
    let dx_left = p.x + half.x;
    let dx_right = half.x - p.x;
    let dy_top = p.y + half.y;
    let dy_bottom = half.y - p.y;
    let min_d = min(min(dx_left, dx_right), min(dy_top, dy_bottom));

    var t = 0.0;
    if dy_top <= min_d + 0.5 {
        // Top edge (left to right)
        t = (p.x + half.x) / perim;
    } else if dx_right <= min_d + 0.5 {
        // Right edge (top to bottom)
        t = (half.x * 2.0 + p.y + half.y) / perim;
    } else if dy_bottom <= min_d + 0.5 {
        // Bottom edge (right to left)
        t = (half.x * 2.0 + half.y * 2.0 + half.x - p.x) / perim;
    } else {
        // Left edge (bottom to top)
        t = 1.0 - (p.y + half.y) / perim;
    }
    return clamp(t, 0.0, 1.0);
}

// Compute border stroke alpha at the node edge.
//
// The border is drawn at the node edge (with a tiny inset for AA clearance).
// Padding provides the gap between border stroke and text content inside.
// `p` is in pixel coords centered on the node.
fn border_stroke_alpha(
    p: vec2<f32>,
    half_size: vec2<f32>,
    insets: vec4<f32>,   // [top, bottom, left, right]
    thickness: f32,
    corner_r: f32,
    kind: f32,
) -> f32 {
    let pad_top = insets.x;
    let pad_bottom = insets.y;
    let pad_left = insets.z;
    let pad_right = insets.w;

    // Border rectangle: inset from node edge.
    // label_inset_top/bottom move the border further in for fieldset/legend labels.
    let label_inset_top = border_stroke.z;
    let label_inset_bottom = border_stroke.w;
    let inset_top = select(1.0, label_inset_top, label_inset_top > 0.0);
    let inset_bottom = select(1.0, label_inset_bottom, label_inset_bottom > 0.0);
    let inset_lr = 1.0;
    // Asymmetric box: offset center and adjust half-size
    let center_y = (inset_top - inset_bottom) * 0.5;
    let border_half = vec2<f32>(
        half_size.x - inset_lr,
        half_size.y - (inset_top + inset_bottom) * 0.5,
    );

    let aa = 1.0; // anti-alias width in pixels

    // bp: border-relative point (offset for asymmetric top/bottom insets)
    let bp = vec2<f32>(p.x, p.y - center_y);

    if kind == BK_TOP_ACCENT {
        // Just the top edge: horizontal line near the top of the node
        let line_y = -border_half.y;
        let line_x0 = -border_half.x;
        let line_x1 = border_half.x;
        let dy = abs(bp.y - line_y);
        let in_x = smoothstep(line_x0 - aa, line_x0, bp.x) * (1.0 - smoothstep(line_x1, line_x1 + aa, bp.x));
        return (1.0 - smoothstep(thickness * 0.5, thickness * 0.5 + aa, dy)) * in_x;
    }

    // SDF at the border rectangle (centered on bp)
    let d = sd_rounded_box(bp, border_half, corner_r);

    // Base stroke: abs(d) < thickness/2 with AA
    var alpha = 1.0 - smoothstep(0.0, aa, abs(d) - thickness * 0.5);

    if kind == BK_OPEN_BOTTOM {
        // Suppress bottom edge + bottom corners. Side edges extend to node bottom.
        let bottom_y = border_half.y - corner_r;
        let bottom_mask = smoothstep(bottom_y, bottom_y + corner_r, bp.y);
        let near_left = abs(bp.x - (-border_half.x)) < thickness;
        let near_right = abs(bp.x - border_half.x) < thickness;
        let is_side_edge = select(0.0, 1.0, near_left || near_right);
        // Side edges: draw as straight vertical lines extending to node bottom
        if is_side_edge > 0.5 && bp.y > bottom_y {
            let side_x = select(border_half.x, -border_half.x, near_left);
            let side_d = abs(bp.x - side_x);
            alpha = 1.0 - smoothstep(0.0, aa, side_d - thickness * 0.5);
        } else {
            alpha *= (1.0 - bottom_mask);
        }
    } else if kind == BK_OPEN_TOP {
        // Suppress top edge + top corners. Side edges extend to node top.
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
        // Horizontal divider line at the top of this block
        let divider_y = -border_half.y;
        let div_d = abs(bp.y - divider_y);
        let div_x0 = -border_half.x;
        let div_x1 = border_half.x;
        let in_x = smoothstep(div_x0 - aa, div_x0, bp.x) * (1.0 - smoothstep(div_x1, div_x1 + aa, bp.x));
        let div_alpha = (1.0 - smoothstep(thickness * 0.5, thickness * 0.5 + aa, div_d)) * in_x;
        alpha = max(alpha, div_alpha);
    } else if kind == BK_DASHED {
        // Modulate stroke with a dash pattern using perimeter parameterization
        let perim = 2.0 * (border_half.x + border_half.y);
        // Approximate perimeter position: project to nearest edge
        var t = 0.0;
        if bp.y <= -border_half.y + aa {
            // Top edge (left to right)
            t = (bp.x + border_half.x) / perim;
        } else if bp.x >= border_half.x - aa {
            // Right edge (top to bottom)
            t = (border_half.x * 2.0 + bp.y + border_half.y) / perim;
        } else if bp.y >= border_half.y - aa {
            // Bottom edge (right to left)
            t = (border_half.x * 2.0 + border_half.y * 2.0 + border_half.x - bp.x) / perim;
        } else {
            // Left edge (bottom to top)
            t = 1.0 - (bp.y + border_half.y) / perim;
        }
        let dash_count = 40.0; // number of dash+gap pairs around perimeter
        let dash_duty = 0.6;   // fraction of each period that is "on"
        let dash_pattern = smoothstep(dash_duty - 0.02, dash_duty + 0.02, fract(t * dash_count));
        alpha *= (1.0 - dash_pattern);
    }

    return alpha;
}

@fragment
fn fragment(in: UiVertexOutput) -> @location(0) vec4<f32> {
    let tex = textureSample(block_texture, block_sampler, in.uv);

    let glow_radius = fx_params.x;
    let glow_intensity = fx_params.y;
    let anim_mode = fx_params.z;
    let corner_r = fx_params.w;
    let tg_radius = text_glow_params.x;
    let has_cursor = cursor_params.z > 0.0;
    let b_thickness = border_stroke.x;
    let b_kind = border_stroke.y;
    let has_border = b_kind > 0.0;

    // Fast path: no effects — pure texture passthrough
    if glow_radius <= 0.0 && tg_radius <= 0.0 && !has_cursor && !has_border {
        return tex;
    }

    let half_size = in.size * 0.5;
    let p = (in.uv - 0.5) * in.size;

    // --- Text glow (composited first, behind everything) ---
    var result = tex;
    if tg_radius > 0.0 {
        let glow_mask = text_glow_alpha(in.uv, tg_radius);
        let intensity = glow_mask * text_glow_color.a;
        let behind = 1.0 - tex.a;
        result = vec4<f32>(
            tex.rgb + text_glow_color.rgb * intensity * behind,
            tex.a + intensity * behind,
        );
    }

    // --- Animation multiplier (shared by border stroke and glow) ---
    var anim = 1.0;
    var chase_bright = 0.0; // Chase brightness for label glyph modulation
    if anim_mode == 1.0 {
        // Breathe
        anim = 0.7 + 0.3 * sin(globals.time);
    } else if anim_mode == 2.0 {
        // Pulse
        anim = 0.4 + 0.6 * sin(globals.time * 3.0);
    } else if anim_mode == 3.0 {
        // Chase: perimeter-based traveling light
        let param = perimeter_param(p, half_size);
        let chase_speed = 0.4;
        let chase_width = 0.15; // fraction of perimeter the bright segment covers
        let wave = fract(param - globals.time * chase_speed);
        chase_bright = smoothstep(chase_width, 0.0, wave);
        anim = chase_bright + 0.15;
    }

    // --- Border stroke (SDF-based, composited behind text, in front of glow) ---
    if has_border {
        var stroke_a = border_stroke_alpha(p, half_size, border_insets, b_thickness, corner_r, b_kind) * anim;

        // Label gap masking: suppress stroke where label text sits.
        // Use label insets (border moved inward for legend-style labels).
        let px_x = p.x + half_size.x; // Convert from centered coords to left-origin
        let li_top = border_stroke.z;
        let li_bottom = border_stroke.w;
        let eff_inset_top = select(1.0, li_top, li_top > 0.0);
        let eff_inset_bottom = select(1.0, li_bottom, li_bottom > 0.0);

        // Top label gap
        if label_gaps.x > 0.0 || label_gaps.y > 0.0 {
            let near_top = p.y < -half_size.y + eff_inset_top + b_thickness;
            if near_top && px_x >= label_gaps.x && px_x <= label_gaps.y {
                stroke_a = 0.0;
            }
        }
        // Bottom label gap
        if label_gaps.z > 0.0 || label_gaps.w > 0.0 {
            let near_bottom = p.y > half_size.y - eff_inset_bottom - b_thickness;
            if near_bottom && px_x >= label_gaps.z && px_x <= label_gaps.w {
                stroke_a = 0.0;
            }
        }

        if stroke_a > 0.0 {
            let bc = border_color.rgb;
            let ba = border_color.a * stroke_a;
            // Composite behind text: only visible where result is transparent
            let behind = 1.0 - result.a;
            result = vec4<f32>(
                result.rgb + bc * ba * behind,
                result.a + ba * behind,
            );
        }
    }

    // --- Chase through label glyphs: brighten label text as the chase wave passes ---
    if chase_bright > 0.0 && has_border {
        let px_x = p.x + half_size.x;
        let cli_top = border_stroke.z;
        let cli_bottom = border_stroke.w;
        let chase_inset_top = select(1.0, cli_top, cli_top > 0.0);
        let chase_inset_bottom = select(1.0, cli_bottom, cli_bottom > 0.0);
        let in_top_gap = (label_gaps.x > 0.0 || label_gaps.y > 0.0)
            && px_x >= label_gaps.x && px_x <= label_gaps.y
            && p.y < -half_size.y + chase_inset_top + b_thickness * 2.0;
        let in_bottom_gap = (label_gaps.z > 0.0 || label_gaps.w > 0.0)
            && px_x >= label_gaps.z && px_x <= label_gaps.w
            && p.y > half_size.y - chase_inset_bottom - b_thickness * 2.0;
        if (in_top_gap || in_bottom_gap) && tex.a > 0.0 {
            // Boost label glyph brightness with the chase wave
            let boost = chase_bright * 0.8;
            result = vec4<f32>(
                result.rgb + result.rgb * boost,
                result.a,
            );
        }
    }

    // --- Cursor beam (sharp rect in UV space, composited over text) ---
    if has_cursor {
        let cx = cursor_params.x;
        let cy = cursor_params.y;
        let cw = cursor_params.z;
        let ch = cursor_params.w;

        if in.uv.x >= cx && in.uv.x <= cx + cw && in.uv.y >= cy && in.uv.y <= cy + ch {
            let ca = cursor_color.a;
            result = vec4<f32>(
                result.rgb * (1.0 - ca) + cursor_color.rgb * ca,
                result.a * (1.0 - ca) + ca,
            );
        }
    }

    // --- Border glow (SDF-based, composited on top) ---
    // Glow emanates from the node edge (same position as border stroke).
    if glow_radius > 0.0 {
        let d = sd_rounded_box(p, half_size, corner_r);
        let edge_glow = exp(d / glow_radius) * glow_intensity * anim;

        let border_glow = glow_color.rgb * edge_glow * (1.0 - result.a);
        let border_alpha = edge_glow * glow_color.a * (1.0 - result.a);

        result = vec4<f32>(
            result.rgb + border_glow,
            max(result.a, border_alpha),
        );
    }

    return result;
}
