//! Per-block texture rendering (MSDF + shader decoration; no vello).
//!
//! Each conversation block renders its own texture. Text blocks (Markdown,
//! Output, PlainText) use MSDF (shader-quality text). ABC renders through
//! MSDF (glyphs) + flat-colored geometry (staff lines, beams, slurs, ties,
//! repeat dots); see `text::msdf::geometry` and
//! `text::msdf::music_geometry_renderer`. SVG rasterizes on the CPU
//! (`text::svg_raster`: usvg + resvg + tiny-skia into a straight-alpha RGBA8
//! buffer, uploaded as a child `Image`/`ImageNode`, sized and re-rasterized
//! from the block's PHYSICAL pixel box — see the `Svg` arm of
//! `build_block_scenes`). Sparklines and the image placeholder are plain Bevy
//! UI geometry (child `Node`+`BackgroundColor` rects), not vello, not a
//! shader. Block cells use `MaterialNode<BlockFxMaterial>` for
//! border/glow/label decoration — role-group dividers ride the same material
//! now (see `sync_role_group_headers`). Bevy's UI system handles scroll,
//! clip, z-order. `BackgroundColor` works again. No coordinate space
//! mismatches.
//!
//! ABC and SVG were the last two `BlockRenderMethod::Vello` producers, and
//! they came off vello on separate branches — this module reached zero only
//! when those two merged. Nothing assigns that variant any more; the
//! conversation view now touches vello solely for Parley text SHAPING
//! (`VelloFont` layout, `Brush` colors), never for rasterization. Retiring
//! the dead variant and its `UiVectorScene` plumbing is tracked in
//! `docs/issues.md`.

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
use crate::text::shaping::{VelloFont, VelloFontAxes, VelloTextAlign, VelloTextStyle};

use crate::cell::block_border::{
    BlockBorderStyle, BlockExcludedState, BorderAnimation, BorderKind, BorderLabelMetrics,
    BorderPadding,
};
use crate::cell::{BlockCell, RoleGroupBorder};
use crate::shaders::BlockFxMaterial;
use crate::text::msdf::{
    BlockRenderMethod, FontDataMap, GeometryVertex, MsdfBlockGeometry, MsdfBlockGlyphs,
    collect_msdf_glyphs, collect_music_geometry, collect_music_glyphs, collect_music_text_glyphs,
    music_geometry_renderer::MusicGeometryRenderer,
    renderer::{ExtractedMsdfAtlas, MsdfBlockRenderer, MsdfBlockUniforms},
};
use crate::text::rich::{RichContent, RichContentKind, SVG_MAX_HEIGHT};
use crate::text::{
    ShapingFonts, KjTextEffects, TextMetrics, bevy_color_to_brush,
};
use crate::text::components::rainbow_brush;
use crate::text::markdown::MarkdownColors;
use crate::text::sparkline::{SparklineColors, SparklineSegment, build_sparkline_geometry};
use crate::text::svg_raster::{fit_svg_to_box, rasterize_svg};
use crate::ui::theme::Theme;
use crate::view::fieldset;
use crate::view::ui_rtt::{UiVectorScene, UiRttTexture, ui_rtt_texture_dims};
use bevy::math::Rot2;
use bevy::ui::UiTransform;

// ============================================================================
// COMPONENTS
// ============================================================================

/// Per-block content + bookkeeping (the build-decision side of a block cell).
///
/// The rasterizable scene + built dimensions live on the sibling
/// [`UiVectorScene`]; this component carries only what the *build* systems need
/// to decide when to rebuild (content versions, formatted text, color). The
/// name is historical — it no longer holds a scene; rename to `BlockContent` is
/// a tracked follow-up.
#[derive(Component)]
pub struct BlockScene {
    /// Content version from sync_block_cell_buffers.
    pub content_version: u64,
    /// Content version that was last built into a scene.
    pub last_built_version: u64,
    /// Monotonic counter bumped each rebuild. Double duty: drives the MSDF
    /// glyph version (= scene_version) and, on the Vello path, is copied into
    /// `UiVectorScene.version` to gate vello rasterization.
    pub scene_version: u64,
    /// Formatted text content (set by sync_block_cell_buffers).
    pub text: String,
    /// Text color (set by sync_block_cell_buffers).
    pub color: Color,
    /// Physical pixel dimensions the `Svg` content-geometry child was last
    /// rasterized at, `(0, 0)` meaning never. Only meaningful while
    /// `RichContent` is `RichContentKind::Svg` — unused (and left stale, but
    /// harmless) otherwise. Lets `build_block_scenes` detect a DPI-only
    /// change (no content edit, no logical-width change) that still leaves
    /// the raster stale, since a CPU raster — unlike vello/MSDF — is only
    /// crisp at the physical size it was rendered for. See `text::svg_raster`.
    pub svg_raster_physical_size: (u32, u32),
}

impl Default for BlockScene {
    fn default() -> Self {
        Self {
            content_version: 0,
            last_built_version: 0,
            scene_version: 0,
            text: String::new(),
            color: Color::WHITE,
            svg_raster_physical_size: (0, 0),
        }
    }
}

/// Child entities a block's content-drawing arm spawned (sparkline polyline,
/// image placeholder background, or the rasterized `Svg` bitmap) that must be
/// despawned before the arm rebuilds them for new content. Sparkline/image
/// children are a plain Bevy UI `Node` + `BackgroundColor` (+ `UiTransform`
/// for rotated stroke segments); the `Svg` child is a `Node` + `ImageNode`
/// sampling a CPU-rasterized `Image` (`text::svg_raster`). No vello, no
/// custom shader in any case — Bevy's own UI rasterizer draws every pixel.
#[derive(Component, Default)]
pub struct ContentGeometryChildren(pub Vec<Entity>);

/// Despawn any content-geometry children left over from a previous build.
/// Called unconditionally at the top of each rebuilt block cell — arms that
/// draw geometry (sparkline, image placeholder) respawn fresh children right
/// after; arms that don't just leave the component removed.
fn clear_content_geometry_children(
    commands: &mut Commands,
    entity: Entity,
    existing: Option<&ContentGeometryChildren>,
) {
    let Some(children) = existing else {
        return;
    };
    for &child in &children.0 {
        commands.entity(child).try_despawn();
    }
    commands.entity(entity).remove::<ContentGeometryChildren>();
}

/// Spawn one axis-aligned rectangle child (fill bar, joint dot, or a plain
/// background panel) at `offset`-relative local content-space coordinates.
fn spawn_rect_child(
    commands: &mut Commands,
    parent: Entity,
    rect: (f32, f32, f32, f32), // x, y, width, height
    offset: (f32, f32),
    color: Color,
) -> Entity {
    let (x, y, w, h) = rect;
    let child = commands
        .spawn((
            Node {
                position_type: PositionType::Absolute,
                left: Val::Px(x + offset.0),
                top: Val::Px(y + offset.1),
                width: Val::Px(w),
                height: Val::Px(h),
                ..default()
            },
            BackgroundColor(color),
        ))
        .id();
    commands.entity(parent).add_child(child);
    child
}

/// Spawn one rotated rectangle child — a sparkline stroke segment — centered
/// at `(segment.cx, segment.cy)` (local content space), `segment.length` px
/// long, `thickness` px tall, rotated `segment.angle` radians about its own
/// center via `UiTransform` (see the pivot note on `SparklineSegment::angle`).
fn spawn_segment_child(
    commands: &mut Commands,
    parent: Entity,
    segment: &SparklineSegment,
    thickness: f32,
    offset: (f32, f32),
    color: Color,
) -> Entity {
    let left = segment.cx - segment.length * 0.5 + offset.0;
    let top = segment.cy - thickness * 0.5 + offset.1;
    let child = commands
        .spawn((
            Node {
                position_type: PositionType::Absolute,
                left: Val::Px(left),
                top: Val::Px(top),
                width: Val::Px(segment.length),
                height: Val::Px(thickness),
                ..default()
            },
            UiTransform::from_rotation(Rot2::radians(segment.angle)),
            BackgroundColor(color),
        ))
        .id();
    commands.entity(parent).add_child(child);
    child
}

/// Spawn a child `Node` + `ImageNode` displaying `image` at `rect` (local
/// content-space coordinates, LOGICAL px — the `Image` asset itself may hold
/// more physical pixels than `rect`'s size implies, on a HiDPI display; that
/// mismatch is exactly what makes it crisp instead of blurry).
fn spawn_image_child(
    commands: &mut Commands,
    parent: Entity,
    image: Handle<Image>,
    rect: (f32, f32, f32, f32), // x, y, width, height
    offset: (f32, f32),
) -> Entity {
    let (x, y, w, h) = rect;
    let child = commands
        .spawn((
            Node {
                position_type: PositionType::Absolute,
                left: Val::Px(x + offset.0),
                top: Val::Px(y + offset.1),
                width: Val::Px(w),
                height: Val::Px(h),
                ..default()
            },
            ImageNode::new(image),
        ))
        .id();
    commands.entity(parent).add_child(child);
    child
}

/// Upload a straight-alpha RGBA8 buffer (see
/// `text::svg_raster::unpremultiply_to_straight_rgba`) as a static sampled
/// `Image` — `Rgba8UnormSrgb` so the GPU treats the bytes as sRGB-encoded
/// color (matching what `resvg` produces) and converts to linear when
/// sampling, same as any asset-loaded PNG/JPEG. Not a render target (contrast
/// `ui_rtt::create_ui_rtt_texture`) — no `RENDER_ATTACHMENT`/`STORAGE_BINDING`
/// usage needed, just sampling.
fn create_svg_raster_image(
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
/// pipeline yet — see the `RichContentKind::Image` arm in `build_block_scenes`).
const IMAGE_PLACEHOLDER_COLOR: Color = Color::srgb(0.2, 0.2, 0.25);

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

        // Main world: build scenes and resize textures in PostUpdate
        // after Taffy layout so ComputedNode.size() is available.
        app.add_systems(
            PostUpdate,
            (
                build_block_scenes.after(bevy::ui::UiSystems::Layout),
                sync_role_group_headers.after(bevy::ui::UiSystems::Layout),
                resize_block_textures
                    .after(build_block_scenes)
                    .after(sync_role_group_headers)
                    .after(crate::view::overlay::build_overlay_glyphs),
            ),
        );

        // Render world: extract and render
        let Some(render_app) = app.get_sub_app_mut(RenderApp) else {
            return;
        };

        // SVG block cells rasterize their vello scene via the generic
        // UiRttPlugin (extract_vello_scenes / render_vello_scenes); this
        // plugin owns only the MSDF compositing pass, which runs *after* the
        // generic vello render so SVG content lands in the texture
        // first (borders are a separate BlockFxMaterial post-process, never
        // part of this texture's contents).
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
                    .after(crate::view::ui_rtt::render_vello_scenes)
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

/// Build each block cell's content: MSDF glyphs for text, MSDF glyphs +
/// flat-colored geometry for ABC notation and diff previews, a CPU raster
/// child for SVG, or plain UI rectangle children for sparkline/image content.
///
/// Runs in PostUpdate after UiSystems::Layout. For each block with changed
/// content or width, rebuilds whichever of those its `RichContent` calls for.
/// Border/glow/label decoration is a separate `BlockFxMaterial` post-process
/// (`shaders::sync_block_fx`), not built here.
pub fn build_block_scenes(
    mut commands: Commands,
    mut block_cells: Query<
        (
            Entity,
            &mut BlockScene,
            &mut UiVectorScene,
            &mut UiRttTexture,
            &ComputedNode,
            &mut Node,
            Option<&RichContent>,
            Option<&BlockBorderStyle>,
            &Visibility,
            Option<&KjTextEffects>,
            &mut MsdfBlockGlyphs,
            &mut MsdfBlockGeometry,
            &mut BlockRenderMethod,
            Option<&BlockExcludedState>,
            Option<&ContentGeometryChildren>,
        ),
        With<BlockCell>,
    >,
    fonts: Res<Assets<VelloFont>>,
    font_handles: Res<ShapingFonts>,
    theme: Res<Theme>,
    text_metrics: Res<TextMetrics>,
    time: Res<Time>,
    mut atlas: Option<ResMut<crate::text::msdf::MsdfAtlas>>,
    mut font_data_map: ResMut<FontDataMap>,
    gpu_limits: Res<GpuTextureLimits>,
    mut images: ResMut<Assets<Image>>,
) {
    let font = fonts.get(&font_handles.mono);

    let md_colors = MarkdownColors {
        heading: theme.md_heading_color,
        code: theme.md_code_fg,
        strong: theme.md_strong_color,
        code_block: theme.md_code_block_fg,
    };

    let sparkline_colors = SparklineColors {
        line: theme.sparkline_line_color,
        fill: theme.sparkline_fill_color,
    };

    // Rainbow phase (cycles 0→1 over ~4 seconds)
    let rainbow_phase = (time.elapsed_secs() * 0.25) % 1.0;

    for (
        entity, mut block_scene, mut ui_scene, mut rtt, computed, mut node, rich, border, vis, effects,
        mut msdf_glyphs, mut msdf_geometry, mut render_method, excluded_state,
        existing_geometry_children,
    ) in block_cells.iter_mut()
    {
        // Skip hidden blocks
        if *vis == Visibility::Hidden {
            continue;
        }

        // Virtualized-out blocks (`virtualize_conversation`) are
        // `Display::None`, which also zeros `ComputedNode.size()` — but
        // check the source (`Node.display`) explicitly rather than relying
        // on the width==0 side effect, so this early-out stays correct even
        // for a legitimately zero-width-but-laid-out node.
        if node.display == Display::None {
            continue;
        }

        // ComputedNode is physical px; all layout below is logical.
        let width = crate::view::ui_rtt::logical_size(computed).x;
        if width <= 0.0 {
            continue;
        }

        // Compute border padding offsets. Moved ahead of the needs_rebuild
        // gate below so `content_width` is available for the `Svg`
        // DPI-staleness check — everything here is cheap and independent of
        // whether a rebuild actually happens.
        let (pad_top, pad_bottom, pad_left, pad_right) = if let Some(style) = border {
            (
                style.padding.top,
                style.padding.bottom,
                style.padding.left,
                style.padding.right,
            )
        } else {
            (0.0, 0.0, 0.0, 0.0)
        };
        let content_width = (width - pad_left - pad_right).max(0.0);

        // Check if rebuild is needed.
        // Rainbow/animation effects are shader-driven (globals.time / uniforms.time)
        // and don't need CPU-side scene rebuilds AFTER the first build. But we must
        // rebuild once when the flag first becomes true so msdf_glyphs.rainbow is set.
        let version_changed = block_scene.content_version != block_scene.last_built_version;
        let width_changed = (rtt.built_width - width).abs() > 1.0;
        let is_rainbow = effects.is_some_and(|e| e.rainbow);
        let has_animation = border.is_some_and(|b| b.animation != BorderAnimation::None);
        let never_built = block_scene.last_built_version == 0;

        // An `Svg` block's CPU raster (`text::svg_raster`) is only crisp at
        // the PHYSICAL pixel size it was rendered for — unlike vello/MSDF
        // content, which the GPU rescales cheaply at render time, a raster
        // bitmap does not adapt on its own. A DPI-only change (window
        // dragged to a different-scale monitor) can leave `width` (logical)
        // and `content_version` both unchanged while still invalidating the
        // raster, so neither `version_changed` nor `width_changed` catches
        // it — check the physical target size directly against what was
        // last rasterized.
        //
        // Capped additionally by `SVG_RASTER_MAX_DIM`, not just the GPU's
        // texture limit: `gpu_limits.max_texture_dim` bounds a GPU texture,
        // but this raster is a plain CPU-side `Vec<u8>` — some GPUs report
        // texture limits (16k+) that would make a straight RGBA8 buffer at
        // that size a multi-hundred-megabyte allocation for a single block.
        let svg_max_dim = gpu_limits
            .max_texture_dim
            .min(crate::text::svg_raster::SVG_RASTER_MAX_DIM);
        let svg_dpi_stale = match rich.map(|r| &r.kind) {
            Some(RichContentKind::Svg { width: svg_w, height: svg_h, .. }) => {
                fit_svg_to_box(*svg_w, *svg_h, content_width, SVG_MAX_HEIGHT)
                    .map(|(_, draw_w, draw_h)| {
                        let target = ui_rtt_texture_dims(
                            draw_w,
                            draw_h,
                            text_metrics.scale_factor,
                            svg_max_dim,
                        );
                        target != block_scene.svg_raster_physical_size
                    })
                    .unwrap_or(false)
            }
            _ => false,
        };

        let needs_rebuild = version_changed
            || width_changed
            || svg_dpi_stale
            || (never_built && (is_rainbow || has_animation));

        if !needs_rebuild {
            continue;
        }

        // Font required for text rendering
        let Some(font) = font else {
            continue;
        };

        let max_advance = if content_width > 0.0 {
            Some(content_width)
        } else {
            None
        };

        // Shed any content-geometry children (sparkline polyline / image
        // placeholder rect) from a previous build — the arms below respawn
        // fresh ones if this build is still one of those kinds.
        clear_content_geometry_children(&mut commands, entity, existing_geometry_children);

        // Shed any flat-colored geometry from a previous build. Two arms
        // populate `MsdfBlockGeometry` (ABC engraving, Diff bands) and both
        // refill it below; every other arm must not inherit it. Geometry has
        // no version of its own — it rides `MsdfBlockGlyphs.version`, which
        // the arms bump — so a stale `vertices` from a block that used to be
        // a diff and is now plain text would keep compositing bands behind
        // the new content. Cheap: a `clear()` on an already-empty Vec.
        msdf_geometry.vertices.clear();

        let scene = vello::Scene::new();
        let content_height: f32;

        // Determine the brush for plain text
        let text_brush = if is_rainbow {
            let alpha = block_scene.color.alpha();
            rainbow_brush(rainbow_phase, alpha)
        } else {
            bevy_color_to_brush(block_scene.color)
        };

        // Build text style
        let style = VelloTextStyle {
            brush: text_brush.clone(),
            font_size: text_metrics.cell_font_size,
            font_axes: VelloFontAxes {
                weight: Some(200.0),
                ..default()
            },
            ..default()
        };

        let text_offset = (pad_left as f64, pad_top as f64);

        match rich.map(|r| &r.kind) {
            Some(RichContentKind::Markdown { spans, plain_text }) => {
                let layout = font.layout(
                    plain_text,
                    &style,
                    VelloTextAlign::Left,
                    max_advance,
                );
                content_height = layout.height();

                let span_brushes = crate::text::rich::build_span_brushes(
                    spans,
                    theme.block_assistant,
                    &md_colors,
                );
                let fallback_brush = bevy_color_to_brush(theme.block_assistant);

                // MSDF renders text; `scene` stays empty for markdown blocks.
                if let Some(ref mut atlas) = atlas {
                    for line in layout.lines() {
                        for item in line.items() {
                            if let parley::PositionedLayoutItem::GlyphRun(gr) = item {
                                font_data_map.register(gr.run().font());
                            }
                        }
                    }
                    let glyphs = collect_msdf_glyphs(
                        &layout, &span_brushes, &fallback_brush, text_offset, atlas,
                    );
                    msdf_glyphs.glyphs = glyphs;
                    msdf_glyphs.version = msdf_glyphs.version.wrapping_add(1);
                    msdf_glyphs.rainbow = is_rainbow;
                    *render_method = BlockRenderMethod::Msdf;
                }
            }
            Some(RichContentKind::Sparkline(data)) => {
                // No vello scene, no shader — the polyline is plain Bevy UI
                // rectangles (thin rotated `Node`s for the stroke, axis-
                // aligned ones for fill/joints), spawned as children below.
                *render_method = BlockRenderMethod::Msdf;

                // A sparkline has NO text of its own, which makes it the one
                // arm that must clear glyphs explicitly. Every text-bearing
                // arm ASSIGNS `msdf_glyphs.glyphs = glyphs`, and that
                // assignment is what drops the previous content's glyphs; an
                // arm that never assigns silently inherits them. The
                // transition is the ordinary streaming path, not an edge
                // case: a block arrives as text, and only when its closing
                // fence lands does `detect_rich_content_typed` reclassify it
                // as a sparkline — so without this the partially-streamed
                // source text stays composited behind the plot.
                msdf_glyphs.glyphs.clear();
                msdf_glyphs.version = block_scene.scene_version.wrapping_add(1);

                let h = theme.sparkline_height;
                content_height = h;

                if content_width > 0.0 && h > 0.0 {
                    let geometry = build_sparkline_geometry(data, content_width, h, 4.0);
                    let offset = (pad_left, pad_top);
                    let mut spawned = Vec::with_capacity(
                        geometry.segments.len() + geometry.joints.len() + geometry.fill_bars.len(),
                    );

                    if let Some(fill_color) = sparkline_colors.fill {
                        for bar in &geometry.fill_bars {
                            spawned.push(spawn_rect_child(
                                &mut commands,
                                entity,
                                (bar.x, bar.y, bar.width, bar.height),
                                offset,
                                fill_color,
                            ));
                        }
                    }
                    for segment in &geometry.segments {
                        spawned.push(spawn_segment_child(
                            &mut commands,
                            entity,
                            segment,
                            crate::text::sparkline::SPARKLINE_STROKE_WIDTH,
                            offset,
                            sparkline_colors.line,
                        ));
                    }
                    for joint in &geometry.joints {
                        spawned.push(spawn_rect_child(
                            &mut commands,
                            entity,
                            (joint.x, joint.y, joint.width, joint.height),
                            offset,
                            sparkline_colors.line,
                        ));
                    }

                    commands.entity(entity).insert(ContentGeometryChildren(spawned));
                }
            }
            Some(RichContentKind::Svg {
                tree,
                width: svg_w,
                height: svg_h,
                ..
            }) => {
                // CPU-rasterized (text::svg_raster), not vello: no scene to
                // build, so this is always an Msdf block — the rasterized
                // bitmap is a plain child ImageNode (below), same shape as
                // the Sparkline/Image arms.
                *render_method = BlockRenderMethod::Msdf;

                // Like the Sparkline arm, Svg has no text of its own — every
                // text-bearing arm ASSIGNS `msdf_glyphs.glyphs`, and it's
                // that assignment (not any explicit cleanup) that drops the
                // previous content's glyphs. Without this, a block that
                // arrives as plain text and is only reclassified as Svg once
                // its closing fence lands would silently inherit the
                // partially-streamed source text, which then composites
                // behind the raster in any transparent/semi-transparent
                // region.
                msdf_glyphs.glyphs.clear();
                msdf_glyphs.version = block_scene.scene_version.wrapping_add(1);

                match fit_svg_to_box(*svg_w, *svg_h, content_width, SVG_MAX_HEIGHT) {
                    None => {
                        // `text::rich::try_parse_svg` already rejects a
                        // non-positive intrinsic SVG size at detection time
                        // (falling through to the plain-text path instead of
                        // ever constructing `RichContentKind::Svg`), so
                        // `svg_w`/`svg_h` are always positive here — the only
                        // way this fires is a degenerate content BOX
                        // (`content_width <= 0`, e.g. a block whose layout
                        // hasn't settled yet). That's a transient, benign
                        // "no room to draw it this frame" — width_changed
                        // will trigger a rebuild once real width lands — not
                        // a content problem, so rendering as empty content
                        // here is correct, not a silent-blank regression.
                        content_height = 0.0;
                        block_scene.svg_raster_physical_size = (0, 0);
                    }
                    Some((fit_scale, draw_w, draw_h)) => {
                        content_height = draw_h;

                        let dpi_scale = text_metrics.scale_factor;
                        let target = ui_rtt_texture_dims(draw_w, draw_h, dpi_scale, svg_max_dim);
                        // `fit_scale` maps SVG user units to LOGICAL px;
                        // chaining the DPI scale on top maps them straight to
                        // PHYSICAL px in one pass, so the raster is sized and
                        // scaled for its actual on-screen resolution rather
                        // than an intermediate logical-res bitmap that then
                        // gets upscaled blurrily.
                        let content_scale = fit_scale * dpi_scale as f64;

                        match rasterize_svg(tree, target.0, target.1, content_scale) {
                            Some(rgba) => {
                                let handle = create_svg_raster_image(
                                    &mut images, target.0, target.1, rgba,
                                );
                                let child = spawn_image_child(
                                    &mut commands,
                                    entity,
                                    handle,
                                    (0.0, 0.0, draw_w, draw_h),
                                    (pad_left, pad_top),
                                );
                                commands
                                    .entity(entity)
                                    .insert(ContentGeometryChildren(vec![child]));
                            }
                            None => {
                                // Defensive only: `target` is clamped to >= 1
                                // by `ui_rtt_texture_dims`, so tiny-skia's
                                // pixmap allocation should never actually
                                // fail here — see `rasterize_svg`'s doc
                                // comment. `svg_raster_physical_size` still
                                // gets updated below so a persistent failure
                                // at a stable size can't force a rebuild
                                // every frame.
                                error!(
                                    "SVG rasterize failed at {}x{} physical px \
                                     (should be unreachable)",
                                    target.0, target.1,
                                );
                            }
                        }

                        block_scene.svg_raster_physical_size = target;
                    }
                }
            }
            Some(RichContentKind::Abc { tune, .. }) => {
                // ABC renders entirely through MSDF + geometry now — no
                // vello scene content at all, same as Markdown/Output/
                // PlainText below. `render_method` stays `Msdf` (the
                // default) so `ui_scene.version` is never bumped for these
                // blocks and the generic vello extract skips their (empty)
                // scene, exactly like every other MSDF-rendered block kind.
                let default_opts = kaijutsu_abc::engrave::EngravingOptions::default();
                let elements =
                    kaijutsu_abc::engrave::layout::engrave(tune, &default_opts);

                let notation_brush = bevy_color_to_brush(theme.block_assistant);
                let bounds =
                    crate::text::abc::compute_engraving_bounds(&elements, default_opts.margin);

                let w_scale = content_width as f64 / bounds.width;
                let h_scale = SVG_MAX_HEIGHT as f64 / bounds.height;
                let scale = w_scale.min(h_scale);
                content_height = (bounds.height * scale) as f32;

                if let Some(ref mut atlas) = atlas {
                    let origin = (bounds.origin_x, bounds.origin_y);

                    let geometry_vertices = collect_music_geometry(
                        &elements,
                        text_offset,
                        origin,
                        scale,
                        &notation_brush,
                    );
                    msdf_geometry.vertices = geometry_vertices;

                    let mut glyphs = collect_music_glyphs(
                        &elements,
                        text_offset,
                        origin,
                        scale,
                        &notation_brush,
                        atlas,
                    );
                    let text_glyphs = collect_music_text_glyphs(
                        &elements,
                        text_offset,
                        origin,
                        scale,
                        &notation_brush,
                        Some(font),
                        atlas,
                        &mut font_data_map,
                    );
                    glyphs.extend(text_glyphs);

                    msdf_glyphs.glyphs = glyphs;
                    // Single shared version gate for BOTH glyphs and
                    // geometry — see `MsdfBlockGeometry`'s doc comment on
                    // why it deliberately carries no version of its own.
                    msdf_glyphs.version = msdf_glyphs.version.wrapping_add(1);
                    msdf_glyphs.rainbow = is_rainbow;
                }
                // No atlas yet (startup ordering): skip this rebuild, same
                // as every other MSDF block kind below — retried on the
                // next content/width change or, in practice, the next
                // frame (the atlas is inserted in `Startup`, before this
                // system ever runs).
            }
            Some(RichContentKind::Diff(preview)) => {
                // Modeled on the Markdown arm — Parley layout + `SpanBrush`
                // coloring — plus ABC's second half: flat-colored geometry
                // for the per-line background bands, built from the same
                // layout and published under the same shared version gate.
                // `text::diff` already spent the render byte budget and the
                // preview line budget before this point; what reaches Parley
                // here is bounded by construction.
                let layout = font.layout(
                    &preview.plain_text,
                    &style,
                    VelloTextAlign::Left,
                    max_advance,
                );
                content_height = layout.height();

                let span_brushes = crate::text::rich::build_diff_span_brushes(preview, &theme);
                // Context is the quietest class, so it is the right fallback
                // for any run a span somehow misses.
                let fallback_brush = bevy_color_to_brush(theme.diff_context_fg);

                if let Some(ref mut atlas) = atlas {
                    for line in layout.lines() {
                        for item in line.items() {
                            if let parley::PositionedLayoutItem::GlyphRun(gr) = item {
                                font_data_map.register(gr.run().font());
                            }
                        }
                    }

                    // One row per WRAPPED visual line, addressed by its start
                    // byte — a long `+` line that wraps must be tinted across
                    // every row it occupies. `min_coord`/`max_coord` are the
                    // line's top/bottom in the same block-local LOGICAL space
                    // the glyphs use.
                    let rows: Vec<crate::text::diff::PreviewRow> = layout
                        .lines()
                        .map(|line| {
                            let m = line.metrics();
                            crate::text::diff::PreviewRow {
                                text_offset: line.text_range().start,
                                top: m.min_coord,
                                bottom: m.max_coord,
                            }
                        })
                        .collect();
                    msdf_geometry.vertices = crate::text::diff::build_diff_band_geometry(
                        preview,
                        &rows,
                        content_width,
                        (pad_left, pad_top),
                        &crate::text::rich::diff_band_colors(&theme),
                    );

                    let glyphs = collect_msdf_glyphs(
                        &layout, &span_brushes, &fallback_brush, text_offset, atlas,
                    );
                    msdf_glyphs.glyphs = glyphs;
                    // Bump from the field's OWN previous value (ABC's
                    // pattern), never from `scene_version` — see
                    // `MsdfBlockGlyphs::version`. Geometry and glyphs ride
                    // this single gate together.
                    msdf_glyphs.version = msdf_glyphs.version.wrapping_add(1);
                    msdf_glyphs.rainbow = is_rainbow;
                    *render_method = BlockRenderMethod::Msdf;
                }
            }
            Some(RichContentKind::Output { layout, plain_text }) => {
                let parley_layout = font.layout(
                    plain_text,
                    &style,
                    VelloTextAlign::Left,
                    max_advance,
                );
                content_height = parley_layout.height();

                let span_brushes =
                    crate::text::rich::build_output_span_brushes(layout, &theme);
                let fallback_brush = bevy_color_to_brush(theme.block_tool_result);

                if let Some(ref mut atlas) = atlas {
                    for line in parley_layout.lines() {
                        for item in line.items() {
                            if let parley::PositionedLayoutItem::GlyphRun(gr) = item {
                                font_data_map.register(gr.run().font());
                            }
                        }
                    }
                    let glyphs = collect_msdf_glyphs(
                        &parley_layout, &span_brushes, &fallback_brush, text_offset, atlas,
                    );
                    msdf_glyphs.glyphs = glyphs;
                    msdf_glyphs.version = msdf_glyphs.version.wrapping_add(1);
                    msdf_glyphs.rainbow = is_rainbow;
                    *render_method = BlockRenderMethod::Msdf;
                }
            }
            Some(RichContentKind::Image { hash }) => {
                // Placeholder: dark rectangle (plain UI geometry, not vello)
                // with an MSDF hash label. Full CAS→decode→ImageNode pipeline
                // requires RpcCommand::CasRead and async image loading, which
                // lands in a follow-up — this only drops vello, the
                // placeholder itself is unchanged.
                *render_method = BlockRenderMethod::Msdf;
                let placeholder_h = 120.0_f32;
                content_height = placeholder_h;

                let rect_child = spawn_rect_child(
                    &mut commands,
                    entity,
                    (0.0, 0.0, content_width, placeholder_h),
                    (pad_left, pad_top),
                    IMAGE_PLACEHOLDER_COLOR,
                );
                commands
                    .entity(entity)
                    .insert(ContentGeometryChildren(vec![rect_child]));

                let label = format!("[image: {}]", &hash[..8.min(hash.len())]);
                let label_layout =
                    font.layout(&label, &style, VelloTextAlign::Left, max_advance);
                let label_y = pad_top + placeholder_h / 2.0 - label_layout.height() / 2.0;
                let label_brush = bevy_color_to_brush(theme.block_tool_result);

                if let Some(ref mut atlas) = atlas {
                    for line in label_layout.lines() {
                        for item in line.items() {
                            if let parley::PositionedLayoutItem::GlyphRun(gr) = item {
                                font_data_map.register(gr.run().font());
                            }
                        }
                    }
                    let glyphs = collect_msdf_glyphs(
                        &label_layout,
                        &[],
                        &label_brush,
                        (pad_left as f64, label_y as f64),
                        atlas,
                    );
                    msdf_glyphs.glyphs = glyphs;
                    // Derive from the field's OWN previous value, never
                    // `scene_version` — see `MsdfBlockGlyphs::version`'s doc
                    // comment for the bug this reintroduces otherwise
                    // (staff-without-glyphs, msdf-music live verify
                    // 2026-07-16). This arm didn't exist when that fix
                    // landed; the devello rewrite of the Image placeholder
                    // re-added the same anti-pattern independently.
                    msdf_glyphs.version = msdf_glyphs.version.wrapping_add(1);
                    msdf_glyphs.rainbow = is_rainbow;
                    *render_method = BlockRenderMethod::Msdf;
                }
            }
            None => {
                // Plain text block
                let layout = font.layout(
                    &block_scene.text,
                    &style,
                    VelloTextAlign::Left,
                    max_advance,
                );
                content_height = layout.height();

                if let Some(ref mut atlas) = atlas {
                    for line in layout.lines() {
                        for item in line.items() {
                            if let parley::PositionedLayoutItem::GlyphRun(gr) = item {
                                font_data_map.register(gr.run().font());
                            }
                        }
                    }
                    let glyphs = collect_msdf_glyphs(
                        &layout, &[], &text_brush, text_offset, atlas,
                    );
                    msdf_glyphs.glyphs = glyphs;
                    msdf_glyphs.version = msdf_glyphs.version.wrapping_add(1);
                    msdf_glyphs.rainbow = is_rainbow;
                    *render_method = BlockRenderMethod::Msdf;
                }
            }
        }

        // Round to physical pixel boundary so built_height matches what Taffy
        // renders. Height is computed by us — without rounding, the MSDF
        // texture NDC has a different aspect ratio than the displayed node,
        // causing subtle vertical stretch on HiDPI displays. `width` itself
        // stays raw here (it still drives `max_advance`/layout above, and
        // label positions below) — only the *stored* `rtt.built_width` gets
        // the same treatment, at the bottom of this function.
        let scale = text_metrics.scale_factor;
        let total_height = round_to_physical_px(content_height + pad_top + pad_bottom, scale);

        // Collect label glyphs for shader-drawn border labels.
        // Labels are baked into the MSDF texture at positions in the border padding zone.
        // This works for both MSDF and Vello blocks — the MSDF pass composites label
        // glyphs on top of whatever content the block has.
        {
            if let (Some(border_style), Some(atlas)) = (border, &mut atlas) {
                let label_font_size = theme.label_font_size;
                let label_inset = theme.label_inset;
                let label_pad = theme.label_pad;

                let label_brush = bevy_color_to_brush(border_style.color);
                let label_style = VelloTextStyle {
                    brush: label_brush.clone(),
                    font_size: label_font_size,
                    ..default()
                };

                let mut metrics = BorderLabelMetrics::default();

                if let Some(ref top_text) = border_style.top_label {
                    let label_layout = font.layout(
                        top_text,
                        &label_style,
                        VelloTextAlign::Left,
                        None,
                    );
                    let label_w = label_layout.width();
                    let label_h = label_layout.height();

                    // Fieldset/legend: move the border inward so the label
                    // straddles it. The inset = ascent/2 + AA clearance puts the
                    // stroke center at exactly ascent/2 from the top edge.
                    let ascent = label_layout
                        .lines()
                        .next()
                        .map(|l| l.metrics().ascent)
                        .unwrap_or(label_h);
                    let inset = (ascent * 0.5 + 1.0).max(2.0);
                    metrics.border_inset_top = inset;
                    // Stroke center is at `inset` from top edge → label_y centers on it
                    let label_x = label_inset + label_pad;
                    let label_y = (inset - ascent * 0.5).max(0.0);
                    let label_offset = (label_x as f64, label_y as f64);

                    for line in label_layout.lines() {
                        for item in line.items() {
                            if let parley::PositionedLayoutItem::GlyphRun(gr) = item {
                                font_data_map.register(gr.run().font());
                            }
                        }
                    }
                    let label_glyphs = collect_msdf_glyphs(
                        &label_layout, &[], &label_brush, label_offset, atlas,
                    );
                    msdf_glyphs.glyphs.extend(label_glyphs);

                    metrics.top_gap_x0 = label_inset;
                    metrics.top_gap_x1 = label_inset + label_pad + label_w + label_pad;
                }

                if let Some(ref bottom_text) = border_style.bottom_label {
                    let label_layout = font.layout(
                        bottom_text,
                        &label_style,
                        VelloTextAlign::Left,
                        None,
                    );
                    let label_w = label_layout.width();
                    let label_h = label_layout.height();

                    // Fieldset/legend: move the bottom border inward too.
                    let ascent = label_layout
                        .lines()
                        .next()
                        .map(|l| l.metrics().ascent)
                        .unwrap_or(label_h);
                    let inset = (ascent * 0.5 + 1.0).max(2.0);
                    metrics.border_inset_bottom = inset;
                    // Stroke center at `total_height - inset`
                    let label_x = (width - label_inset - label_pad - label_w).max(0.0);
                    let label_y = (total_height - inset - ascent * 0.5).max(0.0);
                    let label_offset = (label_x as f64, label_y as f64);

                    for line in label_layout.lines() {
                        for item in line.items() {
                            if let parley::PositionedLayoutItem::GlyphRun(gr) = item {
                                font_data_map.register(gr.run().font());
                            }
                        }
                    }
                    let label_glyphs = collect_msdf_glyphs(
                        &label_layout, &[], &label_brush, label_offset, atlas,
                    );
                    msdf_glyphs.glyphs.extend(label_glyphs);

                    metrics.bottom_gap_x0 = width - label_inset - label_pad - label_w - label_pad;
                    metrics.bottom_gap_x1 = width - label_inset;
                }

                if border_style.top_label.is_some() || border_style.bottom_label.is_some() {
                    commands.entity(entity).insert(metrics);
                }
            }
        }

        // Gutter checkbox: ☑ (included) or ☐ (excluded) in right padding zone.
        // Rendered as an MSDF glyph so it's crisp at any size.
        if let Some(ref mut atlas) = atlas {
            let is_excluded = excluded_state.is_some_and(|e| e.0);
            let checkbox = if is_excluded { "☐" } else { "☑" };
            let cb_font_size = theme.label_font_size;
            let cb_alpha = if is_excluded { 0.5 } else { 0.3 };
            let cb_color = border
                .map(|b| b.color.with_alpha(cb_alpha))
                .unwrap_or(Color::srgba(0.5, 0.55, 0.65, cb_alpha));
            let cb_brush = bevy_color_to_brush(cb_color);
            let cb_style = VelloTextStyle {
                brush: cb_brush.clone(),
                font_size: cb_font_size,
                ..default()
            };
            let cb_layout = font.layout(
                checkbox,
                &cb_style,
                VelloTextAlign::Left,
                None,
            );
            let cb_w = cb_layout.width();
            let cb_h = cb_layout.height();
            // Position: right edge, vertically centered
            let cb_x = (width - cb_w - 4.0).max(0.0);
            let cb_y = ((total_height - cb_h) * 0.5).max(0.0);
            let cb_offset = (cb_x as f64, cb_y as f64);
            for line in cb_layout.lines() {
                for item in line.items() {
                    if let parley::PositionedLayoutItem::GlyphRun(gr) = item {
                        font_data_map.register(gr.run().font());
                    }
                }
            }
            let cb_glyphs = collect_msdf_glyphs(
                &cb_layout, &[], &cb_brush, cb_offset, atlas,
            );
            msdf_glyphs.glyphs.extend(cb_glyphs);
        }

        // Set explicit height on the node
        let target_height = Val::Px(total_height);
        if node.height != target_height {
            node.height = target_height;
        }

        ui_scene.scene = scene;
        // `width` itself (used above for max_advance/layout and label
        // positions) stays the raw Taffy value — only what we store for the
        // RTT texture mapping gets rounded, same reasoning as `total_height`
        // above.
        rtt.built_width = round_to_physical_px(width, scale);
        rtt.built_height = total_height;
        block_scene.last_built_version = block_scene.content_version;
        block_scene.scene_version = block_scene.scene_version.wrapping_add(1);

        // Gate vello rasterization to Vello blocks. MSDF cells leave
        // `ui_scene.version` untouched so the generic extract skips their (empty)
        // scene — `render_msdf_block_textures` clears the texture itself
        // (`has_vello_content == false`). Vello blocks adopt `scene_version` so
        // the extract's `version > last_rendered` gate fires. This reproduces the
        // old `render_method != Msdf` extract skip.
        if *render_method == BlockRenderMethod::Vello {
            ui_scene.version = block_scene.scene_version;
        }
    }
}

/// Sync role-group divider entities: an SDF center-line border (shader) plus
/// an MSDF-rendered role label straddling a gap in it.
///
/// Replaces the old `build_role_group_scenes`, which drew both the line and
/// the label into a per-entity `vello::Scene`. Role headers now carry the
/// same `BlockBorderStyle` + `MsdfBlockGlyphs` + `MaterialNode<BlockFxMaterial>`
/// bundle as an ordinary block cell — `BorderKind::CenterLine` draws the rule
/// (`assets/shaders/block_fx.wgsl`, `sync_block_fx`), and the label glyphs go
/// through the same `collect_msdf_glyphs` path as every other border label.
/// No vello scene is ever built for a role header.
pub fn sync_role_group_headers(
    mut commands: Commands,
    mut role_borders: Query<
        (
            Entity,
            &RoleGroupBorder,
            &mut UiRttTexture,
            &ComputedNode,
            &Node,
            &mut MsdfBlockGlyphs,
            &mut BlockRenderMethod,
            Option<&BlockBorderStyle>,
        ),
        Changed<ComputedNode>,
    >,
    fonts: Res<Assets<VelloFont>>,
    font_handles: Res<ShapingFonts>,
    theme: Res<Theme>,
    text_metrics: Res<TextMetrics>,
    mut atlas: Option<ResMut<crate::text::msdf::MsdfAtlas>>,
    mut font_data_map: ResMut<FontDataMap>,
) {
    let Some(font) = fonts.get(&font_handles.mono) else {
        return;
    };
    let scale = text_metrics.scale_factor;

    for (
        entity, border, mut rtt, computed, node, mut msdf_glyphs, mut render_method, existing_style,
    ) in role_borders.iter_mut()
    {
        // Virtualized-out headers are Display::None; skip explicitly rather
        // than relying on the incidental size.x==0 side effect.
        if node.display == Display::None {
            continue;
        }
        // ComputedNode is physical px; everything below builds in logical.
        let size = crate::view::ui_rtt::logical_size(computed);
        if size.x < 1.0 {
            continue;
        }

        let color = match border.role {
            kaijutsu_crdt::Role::User => theme.block_user,
            kaijutsu_crdt::Role::Model => theme.block_assistant,
            kaijutsu_crdt::Role::System => theme.fg_dim,
            kaijutsu_crdt::Role::Tool | kaijutsu_crdt::Role::Asset => theme.block_tool_call,
        };

        let label = match border.role {
            kaijutsu_crdt::Role::User => "USER",
            kaijutsu_crdt::Role::Model => "ASSISTANT",
            kaijutsu_crdt::Role::System => "SYSTEM",
            kaijutsu_crdt::Role::Tool => "TOOL",
            kaijutsu_crdt::Role::Asset => "ASSET",
        };

        let height = size.y.max(crate::view::lifecycle::ROLE_HEADER_HEIGHT);

        let brush = bevy_color_to_brush(color);
        let label_style = VelloTextStyle {
            brush: brush.clone(),
            font_size: fieldset::ROLE_LABEL_FONT_SIZE,
            ..default()
        };
        let label_layout = font.layout(label, &label_style, VelloTextAlign::Left, None);
        let label_w = label_layout.width() as f64;
        let label_h = label_layout.height();

        let div_layout = fieldset::compute_role_divider_layout(
            label_w,
            theme.label_inset as f64,
            theme.label_pad as f64,
        );
        // Vertically centered on the line, matching the pre-shader Vello
        // fieldset's `text_y = y - line_height / 2`.
        let label_y = ((height as f64 - label_h as f64) * 0.5).max(0.0);

        if let Some(ref mut atlas) = atlas {
            for line in label_layout.lines() {
                for item in line.items() {
                    if let parley::PositionedLayoutItem::GlyphRun(gr) = item {
                        font_data_map.register(gr.run().font());
                    }
                }
            }
            let glyphs = collect_msdf_glyphs(
                &label_layout,
                &[],
                &brush,
                (div_layout.label_x, label_y),
                atlas,
            );
            msdf_glyphs.glyphs = glyphs;
            msdf_glyphs.version = msdf_glyphs.version.wrapping_add(1).max(1);
            *render_method = BlockRenderMethod::Msdf;
        }

        let new_style = BlockBorderStyle {
            kind: BorderKind::CenterLine,
            color,
            thickness: fieldset::ROLE_DIVIDER_THICKNESS,
            corner_radius: 0.0,
            padding: BorderPadding {
                top: 0.0,
                bottom: 0.0,
                left: 0.0,
                right: 0.0,
            },
            animation: BorderAnimation::None,
            top_label: Some(label.to_string()),
            bottom_label: None,
        };
        if existing_style != Some(&new_style) {
            commands.entity(entity).insert(new_style);
        }
        commands.entity(entity).insert(BorderLabelMetrics {
            top_gap_x0: div_layout.gap_x0 as f32,
            top_gap_x1: div_layout.gap_x1 as f32,
            bottom_gap_x0: 0.0,
            bottom_gap_x1: 0.0,
            border_inset_top: 0.0,
            border_inset_bottom: 0.0,
        });

        rtt.built_width = round_to_physical_px(size.x, scale);
        rtt.built_height = round_to_physical_px(height, scale);
    }
}

/// Resize block textures to match physical pixel dimensions.
///
/// Runs after build_block_scenes / sync_role_group_headers so
/// built_width/built_height are up to date. Block cells, the MSDF
/// overlay/shell-dock text surfaces, and role-group divider headers all
/// carry `MaterialNode<BlockFxMaterial>` now, so one system covers all of
/// them — update its texture binding alongside the `ImageNode`.
pub fn resize_block_textures(
    mut block_query: Query<
        (&mut UiRttTexture, &MaterialNode<BlockFxMaterial>, &mut ImageNode),
        Or<(
            With<BlockCell>,
            With<crate::view::components::MsdfOverlayText>,
            With<crate::view::shell_dock::MsdfShellDockText>,
            With<crate::view::editor::render::EditorSurface>,
            With<RoleGroupBorder>,
        )>,
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
        if texture.built_width <= 0.0 || texture.built_height <= 0.0 {
            continue;
        }

        let (target_w, target_h) = crate::view::ui_rtt::ui_rtt_texture_dims(
            texture.built_width,
            texture.built_height,
            scale,
            max_dim,
        );

        if texture.width != target_w || texture.height != target_h {
            let new_handle =
                crate::view::ui_rtt::create_ui_rtt_texture(&mut images, target_w, target_h);
            if let Some(mat) = fx_materials.get_mut(&mat_node.0) {
                mat.texture = new_handle.clone();
            }
            image_node.image = new_handle.clone();
            texture.image = new_handle;
            texture.width = target_w;
            texture.height = target_h;
        }
    }
}

// ============================================================================
// RENDER WORLD SYSTEMS
// ============================================================================

/// Render MSDF geometry + text glyphs to per-block textures.
///
/// Runs after Vello rendering so SVG content is already present in the
/// texture for the blocks that have it (most blocks have none — see
/// `has_vello_content` below; ABC no longer does either, see this module's
/// doc comment). Two composited passes run per item, in order: flat-colored
/// music geometry (staff lines, beams, slurs, ties, repeat dots) first,
/// then MSDF glyphs on top — reproducing the "vello draws lines, MSDF
/// composites glyphs on top" layering ABC used to get from vello, now with
/// geometry as the first layer instead. A block is only considered
/// "settled" (its `version` advances into `last_rendered`, stopping
/// re-extraction) once EVERY non-empty layer it has has rendered
/// successfully this frame — a block with geometry ready but glyphs still
/// atlas-pending must keep re-extracting every frame, not have its pending
/// glyphs silently forgotten because geometry alone "counted" as done.
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
            // Clear only if nothing (vello) drew to this texture already
            // this frame — geometry is always the first MSDF-world layer.
            let clear = !item.has_vello_content;
            if geometry_renderer.encode_render(
                &device,
                &mut encoder,
                &pipeline_cache,
                &gpu_images,
                &item.image_handle,
                &geometry_vertices,
                clear,
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

            // Clear only if NEITHER vello NOR a just-drawn geometry layer
            // put anything in the texture first this frame.
            let clear = !item.has_vello_content && !(has_geometry && geometry_ok);
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
    /// Whether this is an SVG block that vello already rendered into the
    /// texture this frame (false = the texture has no prior content and
    /// MSDF must clear it before compositing; that covers everything else,
    /// ABC and borders included — border decoration is never part of this
    /// texture).
    has_vello_content: bool,
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
            &BlockRenderMethod,
            &UiRttTexture,
        )>,
    >,
) {
    extracted.items.clear();

    for (msdf_glyphs, msdf_geometry, render_method, texture) in query.iter() {
        let asset_id = texture.image.id();
        let last = extracted.last_rendered.get(&asset_id).copied().unwrap_or(0);
        if !should_extract_msdf_block(msdf_glyphs.version, msdf_glyphs.glyphs.is_empty(), last) {
            continue;
        }

        // Vello blocks have content rendered by the Vello pass — MSDF composites on top
        let has_vello = *render_method == BlockRenderMethod::Vello;
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
            has_vello_content: has_vello,
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
