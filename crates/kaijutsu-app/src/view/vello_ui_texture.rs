//! Generic "vello `Scene` → UI `ImageNode` texture" primitive (bevy_vello-escape
//! phase 4).
//!
//! Any UI entity that wants vector content draws a `vello::Scene` into a
//! [`VelloUiScene`] and carries a [`VelloUiTexture`] + `ImageNode`. An extract
//! system ships dirty scenes into the render world; a render system locks the
//! shared [`VelloRasterizer`] and rasterizes each into its texture, scaled from
//! the scene's logical build space to the physical texture. This is the one
//! piece of vello we keep — vello as an offscreen rasterizer presented on an
//! `ImageNode` — generalized out of `block_render` so docks, role-group
//! borders, block cells, and future chrome share one extract/render path
//! instead of each owning a near-identical copy.
//!
//! Consumers own two things the primitive deliberately does *not*:
//! - **scene building** — what to draw, in `built_width`×`built_height` logical
//!   space (a PostUpdate system that writes [`VelloUiScene`] and bumps `version`).
//! - **sizing** — how big the texture should be (a resize system; most call
//!   [`vello_texture_dims`] to turn a logical size + scale factor into clamped
//!   physical dimensions, then realloc via [`create_vello_texture`]).
//!
//! Bumping `version` past the last rendered value is the sole "re-rasterize me"
//! signal; `version == 0` means never-built and is skipped.

use std::collections::HashMap;

use bevy::prelude::*;
use bevy::render::{
    Extract, ExtractSchedule, Render, RenderApp, RenderSystems,
    render_asset::RenderAssets,
    render_resource::{Extent3d, TextureDimension, TextureFormat, TextureUsages},
    renderer::{RenderDevice, RenderQueue},
    texture::GpuImage,
};
use vello::kurbo::Affine;

use crate::view::vello_rasterizer::{VelloRasterizer, VelloRasterizerSettings};

// ============================================================================
// COMPONENTS
// ============================================================================

/// A built `vello::Scene` awaiting rasterization into a sibling [`VelloUiTexture`].
///
/// Consumers build `scene` in `built_width`×`built_height` *logical* pixel space
/// and bump `version`; the render path scales the scene to the texture's physical
/// size, so logical-space coordinates stay DPI-independent.
#[derive(Component)]
pub struct VelloUiScene {
    /// The vector content to rasterize.
    pub scene: vello::Scene,
    /// Logical width the scene was built at (pre scale/clamp).
    pub built_width: f32,
    /// Logical height the scene was built at (pre scale/clamp).
    pub built_height: f32,
    /// Monotonic re-render signal. `0` = never built (skipped by extract); bump
    /// to request a fresh rasterization.
    pub version: u64,
}

impl Default for VelloUiScene {
    fn default() -> Self {
        Self {
            scene: vello::Scene::new(),
            built_width: 0.0,
            built_height: 0.0,
            version: 0,
        }
    }
}

/// The GPU texture a [`VelloUiScene`] rasterizes into (physical pixels), shared
/// with the entity's `ImageNode` (and any material that samples it).
#[derive(Component, Default)]
pub struct VelloUiTexture {
    pub image: Handle<Image>,
    pub width: u32,
    pub height: u32,
}

// ============================================================================
// SIZING + ALLOCATION HELPERS
// ============================================================================

/// Convert a logical size + scale factor into clamped physical texture
/// dimensions. Pure so the sizing math is unit-testable independent of the GPU.
///
/// Each dimension is `ceil(logical * scale)`, clamped to `[1, max_dim]` — never
/// zero (an empty texture is invalid) and never past the GPU/vello tile ceiling.
pub fn vello_texture_dims(
    logical_width: f32,
    logical_height: f32,
    scale: f32,
    max_dim: u32,
) -> (u32, u32) {
    let w = (logical_width * scale).ceil().max(0.0) as u32;
    let h = (logical_height * scale).ceil().max(0.0) as u32;
    (w.clamp(1, max_dim), h.clamp(1, max_dim))
}

/// Create a render-target texture with the format + usage flags vello needs.
pub fn create_vello_texture(images: &mut Assets<Image>, w: u32, h: u32) -> Handle<Image> {
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

// ============================================================================
// RENDER WORLD
// ============================================================================

/// A single extracted scene ready for GPU rasterization.
struct ExtractedVelloSceneItem {
    scene: vello::Scene,
    image_handle: Handle<Image>,
    width: u32,
    height: u32,
    built_width: f32,
    built_height: f32,
    version: u64,
}

/// Render-world buffer of dirty scenes + per-texture last-rendered versions.
#[derive(Resource, Default)]
pub struct ExtractedVelloScenes {
    items: Vec<ExtractedVelloSceneItem>,
    last_rendered: HashMap<AssetId<Image>, u64>,
}

/// Extract dirty [`VelloUiScene`]s from the main world.
///
/// Pushes any scene whose `version` exceeds the last rendered version for its
/// texture (and is non-zero). Mirrors `block_render::extract_block_scenes` minus
/// the block-only MSDF filter — MSDF cells simply leave `version == 0`.
pub fn extract_vello_scenes(
    mut extracted: ResMut<ExtractedVelloScenes>,
    query: Extract<Query<(&VelloUiScene, &VelloUiTexture)>>,
) {
    extracted.items.clear();

    for (scene, texture) in query.iter() {
        if scene.version == 0 {
            continue;
        }

        let asset_id = texture.image.id();
        let last = extracted.last_rendered.get(&asset_id).copied().unwrap_or(0);
        if scene.version > last {
            extracted.items.push(ExtractedVelloSceneItem {
                scene: scene.scene.clone(),
                image_handle: texture.image.clone(),
                width: texture.width,
                height: texture.height,
                built_width: scene.built_width,
                built_height: scene.built_height,
                version: scene.version,
            });
        }
    }
}

/// Rasterize extracted scenes into their textures.
///
/// Locks the shared [`VelloRasterizer`] once and renders all dirty items, each
/// scaled from its logical build space to the physical texture size (handles
/// HiDPI and max-dim clamping alike). Mirrors `block_render::render_block_textures`.
pub fn render_vello_scenes(
    mut extracted: ResMut<ExtractedVelloScenes>,
    renderer: Res<VelloRasterizer>,
    device: Res<RenderDevice>,
    queue: Res<RenderQueue>,
    gpu_images: Res<RenderAssets<GpuImage>>,
    render_settings: Res<VelloRasterizerSettings>,
) {
    if extracted.items.is_empty() {
        return;
    }

    let Ok(mut vello_renderer) = renderer.lock() else {
        warn!("Failed to lock VelloRasterizer for UI scene rendering");
        return;
    };

    let items: Vec<_> = extracted.items.drain(..).collect();

    for item in items {
        let Some(gpu_image) = gpu_images.get(&item.image_handle) else {
            // GpuImage not ready yet — leave last_rendered untouched so next
            // frame's extract re-includes this scene.
            continue;
        };

        if item.width == 0 || item.height == 0 {
            continue;
        }

        let params = vello::RenderParams {
            base_color: vello::peniko::Color::TRANSPARENT,
            width: item.width,
            height: item.height,
            antialiasing_method: render_settings.antialiasing,
        };

        // Scale the scene from logical build space to physical texture pixels.
        let sx = item.width as f64 / item.built_width.max(1.0) as f64;
        let sy = item.height as f64 / item.built_height.max(1.0) as f64;
        let needs_scale = (sx - 1.0).abs() > 0.001 || (sy - 1.0).abs() > 0.001;

        let fitted_scene;
        let scene_to_render = if needs_scale {
            fitted_scene = {
                let mut s = vello::Scene::new();
                s.append(&item.scene, Some(Affine::scale_non_uniform(sx, sy)));
                s
            };
            &fitted_scene
        } else {
            &item.scene
        };

        if let Err(e) = vello_renderer.render_to_texture(
            device.wgpu_device(),
            &queue,
            scene_to_render,
            &gpu_image.texture_view,
            &params,
        ) {
            warn!("UI scene texture render failed: {e}");
            continue;
        }

        extracted
            .last_rendered
            .insert(item.image_handle.id(), item.version);
    }
}

// ============================================================================
// PLUGIN
// ============================================================================

/// Installs the generic extract + render systems into the render world.
///
/// Depends on `VelloRasterizerPlugin` (the shared `vello::Renderer`) already
/// being present. Consumers add their own main-world scene-build + resize
/// systems and spawn entities carrying `VelloUiScene` + `VelloUiTexture` +
/// `ImageNode`.
pub struct VelloUiTexturePlugin;

impl Plugin for VelloUiTexturePlugin {
    fn build(&self, app: &mut App) {
        let Some(render_app) = app.get_sub_app_mut(RenderApp) else {
            return;
        };

        render_app
            .init_resource::<ExtractedVelloScenes>()
            .add_systems(ExtractSchedule, extract_vello_scenes)
            .add_systems(
                Render,
                render_vello_scenes
                    .in_set(RenderSystems::Render)
                    .run_if(|scenes: Res<ExtractedVelloScenes>| !scenes.items.is_empty()),
            );
    }
}

#[cfg(test)]
mod tests {
    use super::vello_texture_dims;

    #[test]
    fn scales_logical_by_factor() {
        // 1x: physical == logical (ceil of exact values).
        assert_eq!(vello_texture_dims(100.0, 40.0, 1.0, 8192), (100, 40));
        // 2x HiDPI doubles both axes.
        assert_eq!(vello_texture_dims(100.0, 40.0, 2.0, 8192), (200, 80));
    }

    #[test]
    fn ceils_fractional_pixels() {
        // 100 * 1.5 = 150 exact; 41 * 1.5 = 61.5 → ceil 62.
        assert_eq!(vello_texture_dims(100.0, 41.0, 1.5, 8192), (150, 62));
    }

    #[test]
    fn clamps_to_max_dim() {
        // A tall block past the GPU/vello ceiling clamps; width stays.
        assert_eq!(vello_texture_dims(1280.0, 20000.0, 1.0, 8192), (1280, 8192));
    }

    #[test]
    fn never_zero() {
        // Zero or sub-pixel logical size still yields a valid 1px texture.
        assert_eq!(vello_texture_dims(0.0, 0.0, 2.0, 8192), (1, 1));
        assert_eq!(vello_texture_dims(0.1, 0.1, 1.0, 8192), (1, 1));
    }
}
