//! Block FX material — MSDF + shader post-processing.
//!
//! `BlockFxMaterial` is a `UiMaterial` that displays the MSDF-rendered block
//! texture and adds GPU-native effects: SDF border stroke + glow, animation
//! overlays, text halo, and cursor beam.

use bevy::prelude::*;
use bevy::render::render_resource::{AsBindGroup, BlendState, RenderPipelineDescriptor};
use bevy::shader::ShaderRef;
use bevy::ui_render::ui_material::UiMaterialKey;

use super::selection::SelectionRects;

/// Post-processing material for conversation block textures.
///
/// # Uniforms
///
/// - `texture` / `sampler`: The MSDF-rendered block texture.
/// - `glow_color`: RGBA color for the border glow effect (linear).
/// - `fx_params`: `[glow_radius, glow_intensity, animation_mode, corner_radius]`
///   animation_mode: 0=none, 1=breathe, 2=pulse, 3=chase
/// - `text_glow_color`: RGBA color for text halo.
/// - `text_glow_params`: `[radius_px, 0, 0, 0]`
/// - `cursor_params`: `[x_uv, y_uv, width_uv, height_uv]` — cursor beam rect in UV space.
///   All zero = no cursor. Color comes from `cursor_color`.
/// - `cursor_color`: RGBA color for the cursor beam (linear).
/// - `border_stroke`: `[thickness_px, border_kind, 0, 0]`
///   border_kind: 0=none, 1=full, 2=top_accent, 3=dashed, 4=open_bottom, 5=open_top,
///   6=center_line (role-group divider — one full-width rule, no box)
/// - `border_insets`: `[pad_top, pad_bottom, pad_left, pad_right]` in pixels.
/// - `border_color`: RGBA color for the border stroke (linear).
/// - `selection_rects`: up to
///   [`MAX_SELECTION_RECTS`](super::selection::MAX_SELECTION_RECTS) UV-space
///   rects plus a live count — a selection is many rectangles, one per visual
///   row it crosses (`super::selection`).
#[derive(Asset, AsBindGroup, TypePath, Debug, Clone)]
pub struct BlockFxMaterial {
    #[texture(0)]
    #[sampler(1)]
    pub texture: Handle<Image>,

    #[uniform(2)]
    pub glow_color: Vec4,

    /// [glow_radius, glow_intensity, animation_mode, corner_radius]
    #[uniform(3)]
    pub fx_params: Vec4,

    /// Text glow color (RGBA, linear color space).
    #[uniform(4)]
    pub text_glow_color: Vec4,

    /// Text glow parameters: [radius_px, excluded_flag, 0, 0].
    /// radius=0 disables glow. excluded_flag: 0.0=included, 1.0=excluded.
    #[uniform(5)]
    pub text_glow_params: Vec4,

    /// Cursor beam rect in UV space: [x, y, width, height]. All zero = disabled.
    #[uniform(6)]
    pub cursor_params: Vec4,

    /// Cursor beam color (RGBA, linear color space).
    #[uniform(7)]
    pub cursor_color: Vec4,

    /// Border stroke: [thickness_px, border_kind, 0, 0].
    /// kind: 0=none, 1=full, 2=top_accent, 3=dashed, 4=open_bottom, 5=open_top.
    #[uniform(8)]
    pub border_stroke: Vec4,

    /// Content insets in pixels: [pad_top, pad_bottom, pad_left, pad_right].
    /// Defines the border zone between node edge and content area.
    #[uniform(9)]
    pub border_insets: Vec4,

    /// Border stroke color (RGBA, linear color space).
    #[uniform(10)]
    pub border_color: Vec4,

    /// Label gap regions (pixel coords): [top_x0, top_x1, bottom_x0, bottom_x1].
    /// Defines horizontal extents where the border stroke is suppressed for labels.
    /// Both x0 and x1 zero = no gap.
    #[uniform(11)]
    pub label_gaps: Vec4,

    /// The selection highlight: **many** rects in UV space, not one.
    ///
    /// A vim Visual selection that crosses a line break covers a ragged first
    /// row, whole middle rows, and a ragged last row — three rects minimum, and
    /// one per row before `super::selection::coalesce_selection_rects` folds
    /// the interior into a single band. `count == 0` skips the composite.
    #[uniform(12)]
    pub selection_rects: SelectionRects,

    /// Selection background color (RGBA, linear color space).
    #[uniform(13)]
    pub selection_color: Vec4,
}

impl Default for BlockFxMaterial {
    fn default() -> Self {
        Self {
            texture: Handle::default(),
            glow_color: Vec4::ZERO,
            fx_params: Vec4::ZERO,
            text_glow_color: Vec4::ZERO,
            text_glow_params: Vec4::ZERO,
            cursor_params: Vec4::ZERO,
            cursor_color: Vec4::ZERO,
            border_stroke: Vec4::ZERO,
            border_insets: Vec4::ZERO,
            border_color: Vec4::ZERO,
            label_gaps: Vec4::ZERO,
            selection_rects: SelectionRects::none(),
            selection_color: Vec4::ZERO,
        }
    }
}

impl UiMaterial for BlockFxMaterial {
    fn fragment_shader() -> ShaderRef {
        "shaders/block_fx.wgsl".into()
    }

    fn specialize(descriptor: &mut RenderPipelineDescriptor, _key: UiMaterialKey<Self>) {
        // The MSDF pass renders glyphs into the block texture with
        // premultiplied alpha, and block_fx.wgsl keeps every composite
        // (glow, border, cursor) premultiplied through to its return value.
        // Bevy's stock UiMaterial pipeline blends with ALPHA_BLENDING
        // (straight alpha, src_factor = SrcAlpha), which multiplies our
        // already-premultiplied fringes by alpha a second time — a dark halo
        // around every glyph. Declare the truth instead.
        if let Some(fragment) = &mut descriptor.fragment
            && let Some(Some(target)) = fragment.targets.first_mut()
        {
            target.blend = Some(BlendState::PREMULTIPLIED_ALPHA_BLENDING);
        }
    }
}
