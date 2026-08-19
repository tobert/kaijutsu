//! Shared MSDF render plumbing (GPU texture sizing + the render-world
//! draw pass) every text surface rides — the conversation surface
//! (`view::surface`), the compose overlay, the shell dock, the editor, and
//! the diff viewer. No vello: MSDF glyphs (`text::msdf`) + flat-colored
//! geometry (ABC engraving, diff bands — `music_geometry_renderer`) render
//! into each surface's own render-to-texture; the conversation view touches
//! vello solely for Parley text SHAPING (`VelloFont` layout, `Brush`
//! colors), never for rasterization.
//!
//! Content itself — deciding what glyphs/geometry a surface needs — is each
//! surface's own job (`view::surface::{content,shape_cache,chrome}` for the
//! conversation; `overlay`/`shell_dock`/`editor`/`diff_view` for the rest).
//! This module owns only what's common past that point: [`GpuTextureLimits`]
//! + [`resize_block_textures`] (sizing every [`MsdfSurface`]'s render
//! target), and the render-world extract/draw pair
//! ([`extract_msdf_blocks`]/[`render_msdf_block_textures`]) that composites
//! whatever glyphs/geometry landed in [`MsdfBlockGlyphs`]/[`MsdfBlockGeometry`]
//! this frame.

use std::collections::HashMap;

use bevy::prelude::*;
use bevy::render::{
    Extract, ExtractSchedule, Render, RenderApp, RenderSystems,
    render_asset::RenderAssets,
    render_resource::{
        CommandEncoder, CommandEncoderDescriptor, Extent3d, PipelineCache, TextureDimension,
        TextureFormat, TextureUsages,
    },
    renderer::{RenderDevice, RenderQueue},
    texture::GpuImage,
};

use crate::shaders::BlockFxMaterial;
use crate::text::msdf::{
    BlockRenderMethod, GeometryVertex, MsdfBlockGeometry, MsdfBlockGlyphs,
    music_geometry_renderer::MusicGeometryRenderer,
    renderer::{ExtractedMsdfAtlas, MsdfBlockRenderer, MsdfBlockUniforms},
};
use crate::text::TextMetrics;
use crate::ui::theme::Theme;
use crate::view::ui_rtt::UiRttTexture;

// ============================================================================
// COMPONENTS
// ============================================================================

/// Per-surface content + bookkeeping (the build-decision side of an
/// MSDF-drawn text surface).
///
/// Carries no rasterizable scene of its own — this component carries what
/// each surface's own build system needs to decide when to rebuild (content
/// version, formatted text, color). Shared by the overlay, shell dock,
/// editor, and diff-view surfaces, each with its own sync system; the
/// conversation surface (`view::surface`) is entity-free and keeps the
/// equivalent state in `content::BlockContentCache` instead. The name is
/// historical; rename to `BlockContent` is a tracked follow-up.
#[derive(Component)]
pub struct BlockScene {
    /// Content version from the owning surface's sync system.
    pub content_version: u64,
    /// Content version that was last built into a scene.
    pub last_built_version: u64,
    /// Formatted text content (set by the owning surface's sync system).
    pub text: String,
    /// Text color (set by the owning surface's sync system).
    pub color: Color,
}

impl Default for BlockScene {
    fn default() -> Self {
        Self {
            content_version: 0,
            last_built_version: 0,
            text: String::new(),
            color: Color::WHITE,
        }
    }
}

/// Marker for every entity `resize_block_textures` must service.
///
/// The system used to match surfaces by an `Or<With<Marker1>, With<Marker2>,
/// ...>` filter, hand-extended one arm per surface type. `Screen::Diff`
/// missed its arm, so `DiffSurface` kept an `ImageNode::default()` placeholder
/// with no `RENDER_ATTACHMENT` usage — a fatal wgpu validation error on first
/// render (2026-08-03, `a4229714`). Attach this marker (via
/// [`msdf_surface_bundle`], never by hand) instead of adding a filter arm: a
/// new surface is then visible to the resize system simply by using the
/// bundle, and the bug class cannot recur by omission.
#[derive(Component, Default)]
pub struct MsdfSurface;

/// The RTT/glyph/material plumbing every hand-rolled MSDF render-texture
/// surface needs, bundled so a new surface cannot forget a piece of it —
/// most importantly [`MsdfSurface`], the marker `resize_block_textures`
/// matches on. Callers add their own extras (`BlockScene`, cursor geometry,
/// border style, `Node`, `Name`, ...); this covers only the part every
/// surface shares: an unbuilt [`UiRttTexture`], empty glyphs, the `Msdf`
/// render method, an `ImageNode` pointing at the un-allocated placeholder,
/// and the `MaterialNode` binding the resize system repoints once it
/// allocates a real texture.
pub fn msdf_surface_bundle(material: Handle<BlockFxMaterial>) -> impl Bundle {
    (
        MsdfSurface,
        UiRttTexture::default(),
        MsdfBlockGlyphs::default(),
        BlockRenderMethod::Msdf,
        // Fully-transparent tint: the ImageNode exists only as the handle
        // slot resize_rtt_texture repoints — the BlockFxMaterial is the sole
        // screen compositor for the texture. A visible tint here makes Bevy
        // draw the texture a second time through the UI image pipeline
        // (straight-alpha, wrong for our premultiplied content), doubling
        // every glyph fringe into a dark halo.
        ImageNode::default().with_color(Color::NONE),
        MaterialNode(material),
    )
}

/// Upload a straight-alpha RGBA8 buffer (see
/// `text::svg_raster::unpremultiply_to_straight_rgba`) as a static sampled
/// `Image` — `Rgba8UnormSrgb` so the GPU treats the bytes as sRGB-encoded
/// color (matching what `resvg` produces) and converts to linear when
/// sampling, same as any asset-loaded PNG/JPEG. Not a render target (contrast
/// `ui_rtt::create_ui_rtt_texture`) — no `RENDER_ATTACHMENT`/`STORAGE_BINDING`
/// usage needed, just sampling.
pub(crate) fn create_svg_raster_image(
    images: &mut Assets<Image>,
    width: u32,
    height: u32,
    rgba: Vec<u8>,
) -> Handle<Image> {
    let size = Extent3d {
        width: width.max(1),
        height: height.max(1),
        depth_or_array_layers: 1,
    };
    let mut image = Image::new(
        size,
        TextureDimension::D2,
        rgba,
        TextureFormat::Rgba8UnormSrgb,
        default(),
    );
    image.texture_descriptor.usage = TextureUsages::TEXTURE_BINDING | TextureUsages::COPY_DST;
    images.add(image)
}

/// Dark background for the image placeholder rect (no real CAS→decode
/// pipeline yet — see the `RichContentKind::Image` arm in
/// `view::surface::rich`).
///
/// `pub(crate)` so the conversation surface draws the identical placeholder
/// (`view::surface::rich`) instead of picking its own shade of "not an image
/// yet".
pub(crate) const IMAGE_PLACEHOLDER_COLOR: Color = Color::srgb(0.2, 0.2, 0.25);

// ============================================================================
// PLUGIN
// ============================================================================

pub struct BlockRenderPlugin;

impl Plugin for BlockRenderPlugin {
    fn build(&self, app: &mut App) {
        // Conservative fallback — finish() will overwrite with the real GPU limit.
        app.insert_resource(GpuTextureLimits {
            max_texture_dim: FALLBACK_MAX_TEXTURE_DIM,
        });

        // Initialize MSDF atlas in main world (needs Assets<Image>)
        app.add_systems(Startup, init_msdf_atlas);

        // Main world: resize textures in PostUpdate after Taffy layout so
        // ComputedNode.size() is available. Content itself is built by each
        // surface's own systems (view::surface, overlay, shell_dock,
        // editor, diff_view) — this plugin owns only the shared texture
        // sizing + render-world extraction/draw every one of them rides.
        app.add_systems(
            PostUpdate,
            resize_block_textures
                .after(bevy::ui::UiSystems::Layout)
                .after(crate::view::overlay::build_overlay_glyphs),
        );

        // Render world: extract and render
        let Some(render_app) = app.get_sub_app_mut(RenderApp) else {
            return;
        };

        // This plugin owns the sole render pass for block cell textures:
        // flat-colored geometry (ABC engraving, Diff bands) then MSDF glyphs
        // on top. SVG is a CPU raster (`text::svg_raster`) composited as a
        // separate child `ImageNode`, never through this texture (borders
        // are a further separate BlockFxMaterial post-process on top of
        // both).
        render_app
            .init_resource::<ExtractedMsdfAtlas>()
            .init_resource::<ExtractedMsdfBlockData>()
            .init_resource::<ExtractedMsdfRenderParams>()
            .add_systems(
                ExtractSchedule,
                (extract_msdf_atlas, extract_msdf_blocks, extract_msdf_render_params),
            )
            .add_systems(
                Render,
                render_msdf_block_textures
                    .in_set(RenderSystems::Render)
                    .run_if(|msdf: Res<ExtractedMsdfBlockData>| !msdf.items.is_empty()),
            );
    }

    fn finish(&self, app: &mut App) {
        // Query the actual GPU texture dimension limit from the render device.
        // RenderDevice isn't available during build(), only after renderer init.
        let gpu_max = app
            .get_sub_app(RenderApp)
            .and_then(|render_app| {
                render_app
                    .world()
                    .get_resource::<RenderDevice>()
                    .map(|d| d.limits().max_texture_dimension_2d)
            })
            .unwrap_or(FALLBACK_MAX_TEXTURE_DIM);

        // Cap at Vello's internal tile limit — the GPU may support larger
        // textures but Vello's coarse workgroup dispatch can't handle them.
        let max_dim = gpu_max.min(VELLO_MAX_TEXTURE_DIM);

        info!(
            "Block texture limit: {} (GPU: {}, Vello cap: {})",
            max_dim, gpu_max, VELLO_MAX_TEXTURE_DIM,
        );

        app.insert_resource(GpuTextureLimits {
            max_texture_dim: max_dim,
        });

        // Initialize MSDF renderer in the render world (needs RenderDevice + PipelineCache).
        if let Some(render_app) = app.get_sub_app_mut(RenderApp) {
            let (renderer, geometry_renderer) = {
                let world = render_app.world();
                let device = world.resource::<RenderDevice>();
                let pipeline_cache = world.resource::<PipelineCache>();
                let asset_server = world.resource::<AssetServer>();
                (
                    MsdfBlockRenderer::init(device, pipeline_cache, asset_server),
                    MusicGeometryRenderer::init(pipeline_cache, asset_server),
                )
            };
            render_app.insert_resource(renderer);
            render_app.insert_resource(geometry_renderer);
            info!("Initialized MSDF block renderer in render world");
        }
    }
}

/// Initialize the MSDF atlas resource (requires Assets<Image>).
fn init_msdf_atlas(mut commands: Commands, mut images: ResMut<Assets<Image>>) {
    let dim = crate::text::msdf::MsdfAtlas::INITIAL_DIM;
    let atlas = crate::text::msdf::MsdfAtlas::new(&mut images, dim, dim);
    commands.insert_resource(atlas);
    info!("Initialized MSDF atlas ({dim}x{dim})");
}

// ============================================================================
// TEXTURE HELPERS
// ============================================================================

/// Fallback max texture dimension when GPU limits aren't available yet.
const FALLBACK_MAX_TEXTURE_DIM: u32 = 8192;

/// Vello's tile-based renderer has an internal limit: the product of coarse
/// workgroup counts (≈ ceil(w/256) * ceil(h/256)) must not exceed 256.
/// For a typical block width of ~1280px that's ceil(1280/256)=5 horizontal tiles,
/// leaving 256/5 ≈ 51 vertical tiles → 51*256 ≈ 13056px max height.
/// Use 8192 as a safe Vello ceiling that works for any reasonable width.
/// See <https://github.com/linebender/vello/issues/680>.
const VELLO_MAX_TEXTURE_DIM: u32 = 8192;

/// Runtime GPU texture dimension limit, queried from the actual device.
///
/// Populated in `BlockRenderPlugin::finish()` from `RenderDevice::limits()`.
/// Falls back to `FALLBACK_MAX_TEXTURE_DIM` if the render device isn't available.
///
/// # Tall block limitation
///
/// Each block renders to a single GPU texture. When a block's pixel height
/// exceeds this limit (e.g. large `find` output, `cat` of a long file),
/// the texture is clamped and `render_block_textures` applies a compensating
/// `Affine::scale_non_uniform` so text displays at correct scale — but with
/// reduced Y resolution. At 2x HiDPI the threshold halves.
///
/// The proper fix is **tiled rendering**: render only the visible portion of
/// tall blocks into a viewport-sized texture, with shader UV remapping.
/// See tech_debt.md item 4 for the full design assessment (~1-2 day lift).
#[derive(Resource, Clone, Copy)]
pub struct GpuTextureLimits {
    pub max_texture_dim: u32,
}

// ============================================================================
// MAIN WORLD SYSTEMS
// ============================================================================

/// Round a logical-pixel dimension so that `logical * scale` lands exactly
/// on a physical pixel boundary, then convert back to logical pixels.
///
/// `UiRttTexture`'s physical texture is sized by
/// [`crate::view::ui_rtt::ui_rtt_texture_dims`] as `ceil(logical *
/// scale)`. A `built_width`/`built_height` that isn't already a multiple of
/// `1/scale` produces a physical texture whose aspect ratio doesn't quite
/// match the logical node's — the MSDF vertex builder maps glyph
/// coordinates into that texture's NDC space assuming an exact match, so
/// any drift here shows up as sub-pixel stretch. Rounding both dimensions
/// through this function keeps `ceil` in `ui_rtt_texture_dims` a no-op
/// (the value is already an integer physical pixel count).
pub(crate) fn round_to_physical_px(logical: f32, scale: f32) -> f32 {
    (logical * scale).round() / scale
}

/// Resize block textures to match physical pixel dimensions.
///
/// Runs after each surface's own content build (the conversation surface,
/// the MSDF overlay/shell-dock/editor/diff text surfaces) so
/// `built_width`/`built_height` are up to date. Every one of those carries
/// [`MsdfSurface`] (via [`msdf_surface_bundle`]), so one filter covers all
/// of them by construction; see that marker's docs for why this is no
/// longer a hand-maintained `Or<With<...>>` list.
pub fn resize_block_textures(
    mut block_query: Query<
        (&mut UiRttTexture, &MaterialNode<BlockFxMaterial>, &mut ImageNode),
        With<MsdfSurface>,
    >,
    text_metrics: Res<TextMetrics>,
    gpu_limits: Res<GpuTextureLimits>,
    mut images: ResMut<Assets<Image>>,
    mut fx_materials: ResMut<Assets<BlockFxMaterial>>,
) {
    let scale = text_metrics.scale_factor;
    let max_dim = gpu_limits.max_texture_dim;

    // Block cells: update material + ImageNode texture bindings.
    // Bevy prepares the GpuImage asynchronously: RenderAssetPlugin reacts to
    // AssetEvent::Modified/Created on the Image asset and (re)creates the
    // wgpu texture on its own schedule, one frame after this mutation lands
    // — there is no synchronous "ensure prepared" call here. Consumers that
    // read the GpuImage this same frame (e.g. render_msdf_block_textures)
    // must tolerate that one-frame gap.
    // MaterialNode's shader reads from the same texture for post-processing.
    for (mut texture, mat_node, mut image_node) in block_query.iter_mut() {
        let (built_width, built_height) = (texture.built_width, texture.built_height);
        let resized = crate::view::ui_rtt::resize_rtt_texture(
            &mut texture,
            &mut image_node,
            built_width,
            built_height,
            scale,
            max_dim,
            &mut images,
        );
        if resized && let Some(mut mat) = fx_materials.get_mut(&mat_node.0) {
            mat.texture = texture.image.clone();
        }
    }
}

// ============================================================================
// RENDER WORLD SYSTEMS
// ============================================================================

/// Render MSDF geometry + text glyphs to per-block textures.
///
/// This is the sole renderer of block cell texture content — no vello pass
/// runs before it. Two composited passes run per item, in order: flat-colored
/// music geometry (staff lines, beams, slurs, ties, repeat dots) first, then
/// MSDF glyphs on top — reproducing the "lines under glyphs" layering ABC
/// used to get from vello, now with geometry as the first MSDF-world layer
/// instead. A block is only considered "settled" (its `version` advances
/// into `last_rendered`, stopping re-extraction) once EVERY non-empty layer
/// it has has rendered successfully this frame — a block with geometry ready
/// but glyphs still atlas-pending must keep re-extracting every frame, not
/// have its pending glyphs silently forgotten because geometry alone
/// "counted" as done.
pub fn render_msdf_block_textures(
    msdf_renderer: Option<Res<MsdfBlockRenderer>>,
    geometry_renderer: Option<Res<MusicGeometryRenderer>>,
    msdf_atlas: Res<ExtractedMsdfAtlas>,
    mut msdf_data: ResMut<ExtractedMsdfBlockData>,
    render_params: Res<ExtractedMsdfRenderParams>,
    device: Res<RenderDevice>,
    queue: Res<RenderQueue>,
    gpu_images: Res<RenderAssets<GpuImage>>,
    pipeline_cache: Res<PipelineCache>,
) {
    let (Some(msdf_renderer), Some(geometry_renderer)) = (msdf_renderer, geometry_renderer) else {
        return;
    };

    let items: Vec<_> = msdf_data.items.drain(..).collect();

    if items.is_empty() {
        return;
    }

    // One encoder for the whole frame's worth of dirty blocks — document-load
    // bursts can dirty dozens of blocks in a single frame, and each used to
    // create its own CommandEncoder and its own `queue.submit`. Encoding
    // every item into one encoder and submitting once collapses that to a
    // single submit per frame (zero submits if nothing actually encoded).
    let mut encoder: CommandEncoder = device.create_command_encoder(&CommandEncoderDescriptor {
        label: Some("msdf_block_batch_encoder"),
    });
    let mut encoded_any = false;

    for item in items {
        // Physical texture size (item.width/height) + the item's OWN
        // logical→physical scale, not the window scale: UI-node surfaces
        // size textures ceil(logical*window_scale) so the two agree, but
        // world-space MSDF panels (time well) allocate 1:1 textures whose
        // scale is 1.0 at any DPI (Fix 3 + per-surface scale — see
        // build_vertices' doc comment and msdf_item_scale).
        let geometry_vertices = MusicGeometryRenderer::build_vertices(
            &item.geometry,
            item.width as f32,
            item.height as f32,
            item.scale,
        );
        // Geometry has no atlas — unlike glyphs it never goes "pending",
        // so `geometry_vertices` is empty iff `item.geometry` was empty.
        let has_geometry = !geometry_vertices.is_empty();

        let glyph_vertices = if item.glyphs.is_empty() {
            Vec::new()
        } else {
            MsdfBlockRenderer::build_vertices(
                &item.glyphs,
                &msdf_atlas,
                item.width as f32,
                item.height as f32,
                item.scale,
                item.rainbow,
            )
        };
        // Unlike geometry, glyphs CAN be source-non-empty but
        // vertices-empty (every glyph is still atlas-pending) — that's a
        // "not ready yet", not a "nothing to draw", case.
        let has_glyphs = !glyph_vertices.is_empty();

        if !has_geometry && !has_glyphs {
            if item.glyphs.is_empty() {
                // Both layers are genuinely empty (not just pending) —
                // clear stale pixels.
                let cleared =
                    msdf_renderer.encode_clear(&mut encoder, &gpu_images, &item.image_handle);
                if cleared {
                    encoded_any = true;
                    msdf_data.last_rendered.insert(item.image_handle.id(), item.version);
                    msdf_data.skip_attempts.remove(&item.image_handle.id());
                } else {
                    // Same skip-tracking as the render path below: a one-frame
                    // GpuImage-not-ready is expected (asset prepare lag), but a
                    // clear that never lands must escalate, not spin silently.
                    let attempts = msdf_data
                        .skip_attempts
                        .entry(item.image_handle.id())
                        .or_insert(0);
                    *attempts += 1;
                    if *attempts > 2 {
                        warn!(
                            "MSDF clear skipped {} consecutive frames: {}x{} target_gpu not ready",
                            attempts, item.width, item.height,
                        );
                    }
                }
            }
            // Else: glyphs exist but are all still atlas-pending — skip
            // silently this frame, naturally retried next frame since
            // `last_rendered` stays behind `item.version`.
            continue;
        }

        // At least one layer has something to draw this frame. Each
        // non-empty layer must independently succeed before the item
        // counts as settled.
        let mut geometry_ok = !has_geometry;
        let mut glyph_ok = !has_glyphs;

        if has_geometry {
            // Geometry is always the first layer drawn to this texture
            // (nothing else precedes it), so it always clears.
            if geometry_renderer.encode_render(
                &device,
                &mut encoder,
                &pipeline_cache,
                &gpu_images,
                &item.image_handle,
                &geometry_vertices,
                true,
            ) {
                geometry_ok = true;
                encoded_any = true;
            }
        }

        if has_glyphs {
            let uniforms = MsdfBlockUniforms {
                resolution: [item.width as f32, item.height as f32],
                msdf_range: msdf_atlas.msdf_range,
                time: render_params.time,
                sdf_texel: [
                    1.0 / msdf_atlas.width as f32,
                    1.0 / msdf_atlas.height as f32,
                ],
                hint_amount: render_params.hint_amount,
                stem_darkening: render_params.stem_darkening,
                horz_scale: render_params.horz_scale,
                vert_scale: render_params.vert_scale,
                text_bias: render_params.text_bias,
                gamma_correction: render_params.gamma_correction,
            };

            // Clear only if a just-drawn geometry layer didn't already put
            // something in the texture first this frame.
            let clear = !(has_geometry && geometry_ok);
            if msdf_renderer.encode_render(
                &device,
                &mut encoder,
                &pipeline_cache,
                &gpu_images,
                &msdf_atlas.texture,
                &item.image_handle,
                &glyph_vertices,
                &uniforms,
                clear,
            ) {
                glyph_ok = true;
                encoded_any = true;
            }
        }

        if geometry_ok && glyph_ok {
            msdf_data
                .last_rendered
                .insert(item.image_handle.id(), item.version);
            msdf_data.skip_attempts.remove(&item.image_handle.id());
        } else {
            // A one-frame `target_gpu=false` here is the KNOWN-benign case:
            // Bevy prepares GpuImage from Image asset changes via
            // AssetEvent::Modified, which lands one frame after the Image
            // is created/resized (there's no synchronous "ensure prepared"
            // path — RenderAssetPlugin's prepare step runs on the render
            // schedule, not inline with the main-world mutation). At
            // document-load scale this fires ~100x/frame across newly
            // spawned blocks, so only escalate to `warn!` once it's
            // persisted past the expected one-frame delay; a single skip is
            // `debug!`.
            let attempts = msdf_data
                .skip_attempts
                .entry(item.image_handle.id())
                .or_insert(0);
            *attempts += 1;

            let geom_pipe_ok = pipeline_cache
                .get_render_pipeline(geometry_renderer.pipeline)
                .is_some();
            let glyph_pipe_ok = pipeline_cache
                .get_render_pipeline(msdf_renderer.pipeline)
                .is_some();
            let target_ok = gpu_images.get(&item.image_handle).is_some();
            let atlas_ok = gpu_images.get(&msdf_atlas.texture).is_some();

            let msg = format!(
                "MSDF render skipped (attempt {}): {}x{} \
                 geometry={}/{} (ok={}) glyphs={}/{} (ok={}) \
                 geom_pipeline={} glyph_pipeline={} target_gpu={} atlas_gpu={}",
                attempts,
                item.width, item.height,
                item.geometry.len(), geometry_vertices.len(), geometry_ok,
                item.glyphs.len(), glyph_vertices.len(), glyph_ok,
                geom_pipe_ok, glyph_pipe_ok, target_ok, atlas_ok,
            );
            if *attempts > 2 {
                warn!("{msg}");
            } else {
                debug!("{msg}");
            }
        }
    }

    if encoded_any {
        queue.submit([encoder.finish()]);
    }
}

// ============================================================================
// MSDF EXTRACTION
// ============================================================================

/// Extracted MSDF block data for the render world.
struct ExtractedMsdfBlockItem {
    glyphs: Vec<crate::text::msdf::PositionedGlyph>,
    /// Flat-colored geometry — ABC engraving (staff lines, beams, slurs,
    /// ties, repeat dots) or Diff background bands; empty for every other
    /// block kind. See `MsdfBlockGeometry`'s doc comment for why this rides
    /// the SAME `version` gate as `glyphs` rather than carrying its own.
    geometry: Vec<GeometryVertex>,
    image_handle: Handle<Image>,
    /// Physical pixel dims of the render target (`ceil(logical * scale)`,
    /// see `ui_rtt::ui_rtt_texture_dims`) — also what `build_vertices` maps
    /// glyph coordinates into (Fix 3; `built_width`/`built_height` on
    /// `UiRttTexture` are logical and no longer needed here).
    width: u32,
    height: u32,
    /// This surface's own logical→physical mapping (`width / built_width`).
    /// UI-node surfaces size their texture `ceil(logical * window_scale)` so
    /// this equals the window scale; world-space MSDF panels (time well
    /// cards/legend) allocate 1:1 textures so it is 1.0 regardless of DPI —
    /// the window scale factor must never be assumed here.
    scale: f32,
    version: u64,
    rainbow: bool,
}

/// A surface's logical→physical scale from its texture + build-space widths.
/// Falls back to 1.0 when the build width is degenerate (never-built panel).
fn msdf_item_scale(tex_width: u32, built_width: f32) -> f32 {
    if built_width > 0.0 {
        tex_width as f32 / built_width
    } else {
        1.0
    }
}

/// Resource holding extracted MSDF block data.
#[derive(Resource, Default)]
pub struct ExtractedMsdfBlockData {
    items: Vec<ExtractedMsdfBlockItem>,
    last_rendered: HashMap<AssetId<Image>, u64>,
    /// Consecutive frames a texture's MSDF render was skipped (target not
    /// GPU-prepared yet, pipeline not ready, etc). Used to keep the
    /// known-benign one-frame `GpuImage` preparation delay at `debug!`
    /// while still escalating to `warn!` if a texture stays stuck.
    skip_attempts: HashMap<AssetId<Image>, u32>,
}

/// Extracted MSDF rendering parameters (from Theme + Time + TextMetrics).
#[derive(Resource, Clone)]
pub struct ExtractedMsdfRenderParams {
    pub hint_amount: f32,
    pub stem_darkening: f32,
    pub horz_scale: f32,
    pub vert_scale: f32,
    pub text_bias: f32,
    pub gamma_correction: f32,
    pub time: f32,
}

impl Default for ExtractedMsdfRenderParams {
    fn default() -> Self {
        Self {
            hint_amount: 0.0,
            stem_darkening: 0.0,
            horz_scale: 0.0,
            vert_scale: 0.0,
            text_bias: 0.0,
            gamma_correction: 0.0,
            time: 0.0,
        }
    }
}

/// Extract MSDF atlas data to the render world.
fn extract_msdf_atlas(
    mut extracted: ResMut<ExtractedMsdfAtlas>,
    atlas: Extract<Option<Res<crate::text::msdf::MsdfAtlas>>>,
) {
    let Some(atlas) = atlas.as_ref() else {
        return;
    };

    if extracted.version == atlas.version {
        return;
    }

    extracted.version = atlas.version;
    extracted.regions = atlas.regions.clone();
    extracted.texture = atlas.texture.clone();
    extracted.width = atlas.width;
    extracted.height = atlas.height;
    extracted.msdf_range = atlas.msdf_range;
}

/// Extract dirty MSDF blocks from the main world.
fn extract_msdf_blocks(
    mut extracted: ResMut<ExtractedMsdfBlockData>,
    query: Extract<
        Query<(
            &MsdfBlockGlyphs,
            Option<&MsdfBlockGeometry>,
            &UiRttTexture,
        )>,
    >,
) {
    extracted.items.clear();

    for (msdf_glyphs, msdf_geometry, texture) in query.iter() {
        let asset_id = texture.image.id();
        let last = extracted.last_rendered.get(&asset_id).copied().unwrap_or(0);
        if !should_extract_msdf_block(msdf_glyphs.version, msdf_glyphs.glyphs.is_empty(), last) {
            continue;
        }

        // `MsdfBlockGeometry` is only ever populated for the block cells that
        // draw flat-colored shapes (ABC engraving, Diff bands) — every other
        // MSDF-glyph-bearing surface (role headers, the shell
        // dock/compose overlay/editor text, time-well cards) has no reason
        // to carry it, and mustn't be REQUIRED to just to satisfy this
        // query: that would silently drop them from extraction entirely
        // (a query mismatch, not a panic) the moment this component exists.
        let geometry_vertices = msdf_geometry.map(|g| g.vertices.clone()).unwrap_or_default();
        extracted.items.push(ExtractedMsdfBlockItem {
            glyphs: msdf_glyphs.glyphs.clone(),
            geometry: geometry_vertices,
            image_handle: texture.image.clone(),
            width: texture.width,
            height: texture.height,
            scale: msdf_item_scale(texture.width, texture.built_width),
            version: msdf_glyphs.version,
            rainbow: msdf_glyphs.rainbow,
        });
    }
}

/// Extract MSDF rendering params (theme + time) to the render world. The
/// logical→physical scale is per-surface (`ExtractedMsdfBlockItem::scale`),
/// not a global here — world-space panels and UI-node surfaces differ.
fn extract_msdf_render_params(
    mut extracted: ResMut<ExtractedMsdfRenderParams>,
    theme: Extract<Res<Theme>>,
    time: Extract<Res<Time>>,
) {
    extracted.hint_amount = theme.msdf_hint_amount;
    extracted.stem_darkening = theme.msdf_stem_darkening;
    extracted.horz_scale = theme.msdf_horz_scale;
    extracted.vert_scale = theme.msdf_vert_scale;
    extracted.text_bias = theme.msdf_text_bias;
    extracted.gamma_correction = theme.msdf_gamma_correction;
    extracted.time = time.elapsed_secs();
}

/// Returns true if an MSDF block should be extracted for rendering this frame.
///
/// A block needs extraction when its version has advanced past what was last rendered.
/// This includes blocks whose glyphs became empty — they still need a clear pass to
/// remove stale texture content. Version 0 means the block was never initialized.
fn should_extract_msdf_block(version: u64, _glyphs_empty: bool, last_rendered: u64) -> bool {
    if version == 0 {
        return false;
    }
    version > last_rendered
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- resize_block_textures marker filter --------------------------------

    /// Every MSDF surface must carry [`MsdfSurface`] (via
    /// [`msdf_surface_bundle`]) or it keeps its `ImageNode::default()`
    /// placeholder — which lacks RENDER_ATTACHMENT, so the first render pass
    /// into it is a fatal wgpu validation error (how `Screen::Diff` crashed
    /// the app on open, 2026-08-03, before the marker filter grew a
    /// `DiffSurface` arm). Spawns `marker` through the real
    /// `msdf_surface_bundle` — the same call every screen's panel-spawn
    /// function makes — so this exercises production code, not a
    /// hand-reimplemented copy of it, and generalizes for free: a future
    /// surface that uses the bundle passes without a new `Or<>` arm
    /// anywhere; one that hand-rolls its components instead is exactly the
    /// mistake this test exists to catch.
    fn assert_surface_gets_renderable_texture(marker: impl Bundle) {
        use bevy::ecs::system::RunSystemOnce;

        let mut world = World::new();
        world.insert_resource(TextMetrics::default());
        world.insert_resource(GpuTextureLimits {
            max_texture_dim: FALLBACK_MAX_TEXTURE_DIM,
        });
        world.insert_resource(Assets::<Image>::default());
        let mut materials = Assets::<BlockFxMaterial>::default();
        let material = materials.add(BlockFxMaterial::default());
        world.insert_resource(materials);

        let surface = world.spawn((marker, msdf_surface_bundle(material))).id();
        // Simulate the post-layout state: a real build pass (sync_editor_text,
        // sync_block_content, ...) stamps a nonzero built size onto the
        // surface spawned above before resize ever runs.
        world.get_mut::<crate::view::ui_rtt::UiRttTexture>(surface).unwrap().built_width = 640.0;
        world.get_mut::<crate::view::ui_rtt::UiRttTexture>(surface).unwrap().built_height = 480.0;

        world
            .run_system_once(resize_block_textures)
            .expect("system runs");

        let rtt = world.get::<crate::view::ui_rtt::UiRttTexture>(surface).unwrap();
        assert!(
            rtt.width > 0 && rtt.height > 0,
            "surface not matched by resize_block_textures' MsdfSurface filter"
        );
        let image_node = world.get::<ImageNode>(surface).unwrap();
        let images = world.resource::<Assets<Image>>();
        let image = images
            .get(&image_node.image)
            .expect("ImageNode still points at the un-allocated placeholder");
        assert!(
            image
                .texture_descriptor
                .usage
                .contains(TextureUsages::RENDER_ATTACHMENT),
            "allocated texture is not renderable: {:?}",
            image.texture_descriptor.usage
        );
    }

    #[test]
    fn diff_surface_gets_renderable_texture() {
        assert_surface_gets_renderable_texture(crate::view::diff_view::render::DiffSurface);
    }

    #[test]
    fn editor_surface_gets_renderable_texture() {
        assert_surface_gets_renderable_texture(crate::view::editor::render::EditorSurface);
    }

    #[test]
    fn overlay_surface_gets_renderable_texture() {
        assert_surface_gets_renderable_texture(crate::view::components::MsdfOverlayText);
    }

    #[test]
    fn shell_dock_surface_gets_renderable_texture() {
        assert_surface_gets_renderable_texture(crate::view::shell_dock::MsdfShellDockText);
    }

    // -- round_to_physical_px (Fix 2: built_width physical-pixel rounding) -
    //
    // `rtt.built_height` was already rounded this way (the NDC-aspect-
    // mismatch comment above `total_height` predates this fix); `built_width`
    // stored the raw fractional Taffy width, which `ui_rtt_texture_dims`
    // (ceil(logical * scale)) and the MSDF vertex builder (divides by the
    // *stored* built_width/height, not the physical texture size — see Fix 3)
    // both assume is already physical-pixel-exact.

    #[test]
    fn round_to_physical_px_is_exact_at_integer_scale() {
        // Scale 1.0: every logical pixel is already a physical pixel, so a
        // fractional Taffy width (e.g. from flex remainder distribution)
        // still needs the round — this is the exact scale-1.0 HiDPI-off case
        // the bug report covers.
        assert_eq!(round_to_physical_px(100.0, 1.0), 100.0);
        assert_eq!(round_to_physical_px(100.4, 1.0), 100.0);
        assert_eq!(round_to_physical_px(100.6, 1.0), 101.0);
    }

    #[test]
    fn round_to_physical_px_lands_on_a_physical_pixel_at_fractional_scale() {
        // The bug this fixes: a fractional `scale` (common HiDPI values
        // like 1.25, 1.5) turns an un-rounded logical width into a
        // fractional physical pixel count, which is what corrupted the NDC
        // aspect ratio (`ui_rtt_texture_dims` then `ceil()`s a non-integer,
        // and the vertex builder's implicit "built_width*scale ==
        // texture_width" assumption breaks).
        for &(logical, scale) in &[
            (100.4_f32, 1.25_f32),
            (247.3, 1.5),
            (63.999, 2.0),
            (1.0, 1.3333334), // 4/3 — a genuinely awkward scale factor
        ] {
            let rounded = round_to_physical_px(logical, scale);
            let physical = rounded * scale;
            assert!(
                (physical - physical.round()).abs() < 1e-3,
                "round_to_physical_px({logical}, {scale}) = {rounded} -> \
                 physical {physical} is not integer-pixel-aligned",
            );
        }
    }

    // -- msdf_item_scale ----------------------------------------------------

    #[test]
    fn msdf_item_scale_is_window_scale_for_ui_surfaces() {
        // UI-node surfaces allocate ceil(logical * window_scale) textures,
        // so the per-item scale recovers the window scale exactly.
        assert_eq!(msdf_item_scale(512, 256.0), 2.0);
        assert_eq!(msdf_item_scale(384, 256.0), 1.5);
    }

    #[test]
    fn msdf_item_scale_is_one_for_world_space_panels() {
        // Time-well cards/legend allocate 1:1 textures (world-space quads):
        // their glyphs must NOT scale with window DPI — the regression that
        // made card text overflow its board on the 4k machine.
        assert_eq!(msdf_item_scale(256, 256.0), 1.0);
    }

    #[test]
    fn msdf_item_scale_survives_degenerate_build_width() {
        assert_eq!(msdf_item_scale(256, 0.0), 1.0);
    }

    // -- should_extract_msdf_block -----------------------------------------

    #[test]
    fn extract_normal_glyphs_with_advanced_version() {
        // Standard case: non-empty glyphs, version ahead of last render.
        assert!(should_extract_msdf_block(3, false, 2));
    }

    #[test]
    fn skip_when_version_zero() {
        // Initial state — never had glyphs, nothing to render or clear.
        assert!(!should_extract_msdf_block(0, true, 0));
        assert!(!should_extract_msdf_block(0, false, 0));
    }

    #[test]
    fn skip_when_already_rendered() {
        // Version matches last_rendered — no work needed.
        assert!(!should_extract_msdf_block(5, false, 5));
    }

    #[test]
    fn extract_empty_glyphs_when_version_advanced() {
        // THE BUG: text was "aaa", user backspaced to empty.
        // Version advanced (text changed) but glyphs are now empty.
        // Must still extract so the render pass can clear stale glyph pixels.
        assert!(should_extract_msdf_block(6, true, 3));
    }

    #[test]
    fn skip_empty_glyphs_already_cleared() {
        // Empty glyphs at a version we already rendered (cleared).
        // No need to re-clear.
        assert!(!should_extract_msdf_block(6, true, 6));
    }
}
