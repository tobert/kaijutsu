//! Generic "render-to-texture → UI `ImageNode`" primitive.
//!
//! Any UI entity that wants offscreen-rasterized content carries a
//! [`UiRttTexture`] (the GPU render target + the logical size its content was
//! authored in). Today the only content that lands in one is MSDF glyphs
//! (see `text::msdf`) — block cells, role headers, and the North/South dock
//! chrome (`ui::dock`) all render into a `UiRttTexture` through the generic
//! MSDF extract/render pass (`view::block_render::{extract_msdf_blocks,
//! render_msdf_block_textures}`), not through anything in this module.
//!
//! This module was also vello's offscreen-rasterizer home (a `UiVectorScene`
//! carrying a `vello::Scene`, extracted and rasterized by
//! `VelloRasterizer`) until the dock chrome — the last consumer — moved onto
//! MSDF (retiring vello, docs/issues.md, 2026-08-12). What's left is purely
//! the content-neutral texture primitive + its sizing helpers.
//!
//! Consumers own two things the primitive deliberately does *not*:
//! - **content building** — what the texture holds (each consumer's own
//!   PostUpdate system).
//! - **sizing** — how big the texture should be (a resize system; most call
//!   [`ui_rtt_texture_dims`] to turn a logical size + scale factor into clamped
//!   physical dimensions, then realloc via [`create_ui_rtt_texture`]).

use bevy::prelude::*;
use bevy::render::{
    render_resource::{Extent3d, TextureDimension, TextureFormat, TextureUsages},
};

// ============================================================================
// COMPONENTS
// ============================================================================

/// The GPU render-target texture an entity's content rasterizes into (physical
/// pixels), shared with the entity's `ImageNode` (and any material that samples
/// it).
///
/// Also carries the **logical** size the content was authored in
/// (`built_width`×`built_height`); the render paths scale from this logical
/// space to the physical texture, so authoring coordinates stay DPI-independent.
#[derive(Component, Default)]
pub struct UiRttTexture {
    pub image: Handle<Image>,
    /// Physical texture width (px).
    pub width: u32,
    /// Physical texture height (px).
    pub height: u32,
    /// Logical width the content was built at (pre scale/clamp).
    pub built_width: f32,
    /// Logical height the content was built at (pre scale/clamp).
    pub built_height: f32,
}

// ============================================================================
// SIZING + ALLOCATION HELPERS
// ============================================================================

/// Convert a logical size + scale factor into clamped physical texture
/// dimensions. Pure so the sizing math is unit-testable independent of the GPU.
///
/// Each dimension is `ceil(logical * scale)`, clamped to `[1, max_dim]` — never
/// zero (an empty texture is invalid) and never past the GPU's texture ceiling.
pub fn ui_rtt_texture_dims(
    logical_width: f32,
    logical_height: f32,
    scale: f32,
    max_dim: u32,
) -> (u32, u32) {
    let w = (logical_width * scale).ceil().max(0.0) as u32;
    let h = (logical_height * scale).ceil().max(0.0) as u32;
    (w.clamp(1, max_dim), h.clamp(1, max_dim))
}

/// A laid-out node's size in LOGICAL pixels.
///
/// bevy_ui 0.18's [`ComputedNode`] reports size/content/border/padding in
/// PHYSICAL pixels, while everything the app authors in — font sizes,
/// `Val::Px`, `ScrollPosition`, `built_width`/`built_height` — is logical.
/// Every read of a `ComputedNode` dimension must convert through this (or
/// [`logical_content_size`]) or HiDPI screens get double-scaled layout math
/// (tall-narrow glyphs, half-size dock text — see devlog).
pub fn logical_size(computed: &bevy::ui::ComputedNode) -> Vec2 {
    computed.size() * computed.inverse_scale_factor()
}

/// A laid-out node's content-box size in LOGICAL pixels (see [`logical_size`]).
pub fn logical_content_size(computed: &bevy::ui::ComputedNode) -> Vec2 {
    let cb = computed.content_box();
    Vec2::new(cb.width(), cb.height()) * computed.inverse_scale_factor()
}

/// Create a render-target texture with the format + usage flags the MSDF
/// render pass needs.
pub fn create_ui_rtt_texture(images: &mut Assets<Image>, w: u32, h: u32) -> Handle<Image> {
    let size = Extent3d {
        width: w.max(1),
        height: h.max(1),
        depth_or_array_layers: 1,
    };
    let mut image = Image::new(
        size,
        TextureDimension::D2,
        vec![0u8; (size.width * size.height * 4) as usize],
        TextureFormat::Rgba8Unorm,
        default(),
    );
    image.texture_descriptor.usage = TextureUsages::TEXTURE_BINDING
        | TextureUsages::COPY_DST
        | TextureUsages::STORAGE_BINDING
        | TextureUsages::RENDER_ATTACHMENT;
    images.add(image)
}

/// Reallocate `texture`'s backing image (and repoint `image_node`) to match
/// `logical_width`×`logical_height` scaled/clamped to physical px, if that
/// target differs from what's currently allocated. Returns `true` when it
/// actually resized (so `texture.image` now points at a fresh handle);
/// `false` when the current allocation already matches, or the requested
/// logical size is degenerate (`<= 0` on either axis) — in both cases
/// nothing is touched.
///
/// Shared by `block_render::resize_block_textures` (sizes from each
/// surface's own `built_width`/`built_height`, set by that surface's own
/// build pass) and `dock::resize_dock_textures` (sizes from the dock's
/// `ComputedNode` directly, every frame — the dock has no separate "build"
/// pass gating when its layout size is trustworthy, so it reads live layout
/// instead of a cached built size). That's a real difference in *what* each
/// caller feeds this, not a stylistic one; this function owns only the
/// "compute target dims, compare, reallocate, swap handles" plumbing that
/// was identical either way.
pub fn resize_rtt_texture(
    texture: &mut UiRttTexture,
    image_node: &mut ImageNode,
    logical_width: f32,
    logical_height: f32,
    scale: f32,
    max_dim: u32,
    images: &mut Assets<Image>,
) -> bool {
    if logical_width <= 0.0 || logical_height <= 0.0 {
        return false;
    }

    let (target_w, target_h) = ui_rtt_texture_dims(logical_width, logical_height, scale, max_dim);
    if texture.width == target_w && texture.height == target_h {
        return false;
    }

    let new_handle = create_ui_rtt_texture(images, target_w, target_h);
    image_node.image = new_handle.clone();
    texture.image = new_handle;
    texture.width = target_w;
    texture.height = target_h;
    true
}

#[cfg(test)]
mod tests {
    use super::{logical_content_size, logical_size, ui_rtt_texture_dims};
    use bevy::prelude::Vec2;
    use bevy::ui::ComputedNode;

    #[test]
    fn logical_size_undoes_hidpi_scale() {
        // ComputedNode is physical px; at scale 2 (inverse 0.5) a 200×100
        // physical node is 100×50 logical. This is the unit conversion every
        // ComputedNode read must make — regression guard for the 4k
        // tall-narrow-glyph bug.
        let node = ComputedNode {
            size: Vec2::new(200.0, 100.0),
            inverse_scale_factor: 0.5,
            ..Default::default()
        };
        assert_eq!(logical_size(&node), Vec2::new(100.0, 50.0));
        // content_box == size here (no border/padding/scrollbar).
        assert_eq!(logical_content_size(&node), Vec2::new(100.0, 50.0));
    }

    #[test]
    fn logical_size_is_identity_at_1x() {
        let node = ComputedNode {
            size: Vec2::new(123.0, 45.0),
            inverse_scale_factor: 1.0,
            ..Default::default()
        };
        assert_eq!(logical_size(&node), Vec2::new(123.0, 45.0));
    }

    #[test]
    fn scales_logical_by_factor() {
        // 1x: physical == logical (ceil of exact values).
        assert_eq!(ui_rtt_texture_dims(100.0, 40.0, 1.0, 8192), (100, 40));
        // 2x HiDPI doubles both axes.
        assert_eq!(ui_rtt_texture_dims(100.0, 40.0, 2.0, 8192), (200, 80));
    }

    #[test]
    fn ceils_fractional_pixels() {
        // 100 * 1.5 = 150 exact; 41 * 1.5 = 61.5 → ceil 62.
        assert_eq!(ui_rtt_texture_dims(100.0, 41.0, 1.5, 8192), (150, 62));
    }

    #[test]
    fn clamps_to_max_dim() {
        // A tall block past the GPU/vello ceiling clamps; width stays.
        assert_eq!(ui_rtt_texture_dims(1280.0, 20000.0, 1.0, 8192), (1280, 8192));
    }

    #[test]
    fn never_zero() {
        // Zero or sub-pixel logical size still yields a valid 1px texture.
        assert_eq!(ui_rtt_texture_dims(0.0, 0.0, 2.0, 8192), (1, 1));
        assert_eq!(ui_rtt_texture_dims(0.1, 0.1, 1.0, 8192), (1, 1));
    }
}
