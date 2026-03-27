//! Per-block texture rendering (Vello + MSDF).
//!
//! Each conversation block renders its own texture. Text blocks use MSDF
//! (shader-quality text), while SVG/sparkline/ABC use Vello (vector rasterizer).
//! Block cells use `MaterialNode<BlockFxMaterial>` for hybrid effects.
//! Role group borders use plain `ImageNode`. Bevy's UI system handles scroll,
//! clip, z-order. `BackgroundColor` works again. No coordinate space mismatches.

use std::collections::HashMap;

use bevy::prelude::*;
use bevy::render::{
    Extract, ExtractSchedule, Render, RenderApp, RenderSystems,
    render_asset::RenderAssets,
    render_resource::{Extent3d, PipelineCache, TextureDimension, TextureFormat, TextureUsages},
    renderer::{RenderDevice, RenderQueue},
    texture::GpuImage,
};
use bevy_vello::prelude::*;
use bevy_vello::vello;
use vello::kurbo::Affine;
use vello::peniko::Fill;

use crate::cell::block_border::{BlockBorderStyle, BorderAnimation, BorderLabelMetrics};
use crate::cell::{BlockCell, RoleGroupBorder};
use crate::shaders::BlockFxMaterial;
use crate::text::msdf::{
    BlockRenderMethod, FontDataMap, MsdfBlockGlyphs,
    collect_msdf_glyphs,
    renderer::{ExtractedMsdfAtlas, MsdfBlockRenderer, MsdfBlockUniforms},
};
use crate::text::rich::{RichContent, RichContentKind, SVG_MAX_HEIGHT};
use crate::text::{
    FontHandles, KjTextEffects, TextMetrics, bevy_color_to_brush,
};
use crate::text::components::rainbow_brush;
use crate::text::markdown::MarkdownColors;
use crate::text::sparkline::{SparklineColors, build_sparkline_paths, render_sparkline_scene};
use crate::ui::theme::Theme;
use crate::view::fieldset;

// ============================================================================
// COMPONENTS
// ============================================================================

/// Per-block rendering state. Stores the built vello scene and metadata.
#[derive(Component)]
pub struct BlockScene {
    /// The complete vello scene: text + border + rich content.
    pub scene: vello::Scene,
    /// Content version from sync_block_cell_buffers.
    pub content_version: u64,
    /// Content version that was last built into a scene.
    pub last_built_version: u64,
    /// Monotonic counter bumped each time the scene is rebuilt.
    pub scene_version: u64,
    /// Logical width the scene was built at.
    pub built_width: f32,
    /// Logical height from text measurement (includes border padding).
    pub built_height: f32,
    /// Formatted text content (set by sync_block_cell_buffers).
    pub text: String,
    /// Text color (set by sync_block_cell_buffers).
    pub color: Color,
}

impl Default for BlockScene {
    fn default() -> Self {
        Self {
            scene: vello::Scene::new(),
            content_version: 0,
            last_built_version: 0,
            scene_version: 0,
            built_width: 0.0,
            built_height: 0.0,
            text: String::new(),
            color: Color::WHITE,
        }
    }
}

/// Per-block GPU texture handle and dimensions (physical pixels).
#[derive(Component)]
pub struct BlockTexture {
    pub image: Handle<Image>,
    pub width: u32,
    pub height: u32,
}

// ============================================================================
// RENDER WORLD TYPES
// ============================================================================

/// A single extracted block scene ready for GPU rendering.
struct ExtractedBlockSceneItem {
    scene: vello::Scene,
    image_handle: Handle<Image>,
    width: u32,
    height: u32,
    /// Logical width the scene was built at (before scale/clamp).
    built_width: f32,
    /// Logical height the scene was built at (before scale/clamp).
    built_height: f32,
    version: u64,
}

/// Resource holding extracted block scenes and version tracking.
#[derive(Resource, Default)]
pub struct ExtractedBlockScenes {
    items: Vec<ExtractedBlockSceneItem>,
    /// Last successfully rendered version per image handle.
    last_rendered: HashMap<AssetId<Image>, u64>,
}

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
                build_role_group_scenes.after(bevy::ui::UiSystems::Layout),
                resize_block_textures
                    .after(build_block_scenes)
                    .after(build_role_group_scenes)
                    .after(crate::view::overlay::build_overlay_glyphs),
            ),
        );

        // Render world: extract and render
        let Some(render_app) = app.get_sub_app_mut(RenderApp) else {
            return;
        };

        render_app
            .init_resource::<ExtractedBlockScenes>()
            .init_resource::<ExtractedMsdfAtlas>()
            .init_resource::<ExtractedMsdfBlockData>()
            .init_resource::<ExtractedMsdfRenderParams>()
            .add_systems(
                ExtractSchedule,
                (
                    extract_block_scenes,
                    extract_msdf_atlas,
                    extract_msdf_blocks,
                    extract_msdf_render_params,
                ),
            )
            .add_systems(
                Render,
                (
                    render_block_textures
                        .in_set(RenderSystems::Render)
                        .run_if(|scenes: Res<ExtractedBlockScenes>| !scenes.items.is_empty()),
                    render_msdf_block_textures
                        .in_set(RenderSystems::Render)
                        .after(render_block_textures)
                        .run_if(|msdf: Res<ExtractedMsdfBlockData>| !msdf.items.is_empty()),
                ),
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
            let renderer = {
                let world = render_app.world();
                let device = world.resource::<RenderDevice>();
                let pipeline_cache = world.resource::<PipelineCache>();
                let asset_server = world.resource::<AssetServer>();
                MsdfBlockRenderer::init(device, pipeline_cache, asset_server)
            };
            render_app.insert_resource(renderer);
            info!("Initialized MSDF block renderer in render world");
        }
    }
}

/// Initialize the MSDF atlas resource (requires Assets<Image>).
fn init_msdf_atlas(mut commands: Commands, mut images: ResMut<Assets<Image>>) {
    let atlas = crate::text::msdf::MsdfAtlas::new(&mut images, 1024, 1024);
    commands.insert_resource(atlas);
    info!("Initialized MSDF atlas (1024x1024)");
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

/// Create a render-target texture with the required format and usage flags.
pub fn create_block_texture(images: &mut Assets<Image>, w: u32, h: u32) -> Handle<Image> {
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
// MAIN WORLD SYSTEMS
// ============================================================================

/// Build vello scenes for block cells.
///
/// Runs in PostUpdate after UiSystems::Layout. For each block with changed
/// content or width, builds a complete vello scene (text + rich content + border).
pub fn build_block_scenes(
    mut commands: Commands,
    mut block_cells: Query<
        (
            Entity,
            &mut BlockScene,
            &ComputedNode,
            &mut Node,
            Option<&RichContent>,
            Option<&BlockBorderStyle>,
            &Visibility,
            Option<&KjTextEffects>,
            &mut MsdfBlockGlyphs,
            &mut BlockRenderMethod,
        ),
        With<BlockCell>,
    >,
    fonts: Res<Assets<VelloFont>>,
    font_handles: Res<FontHandles>,
    theme: Res<Theme>,
    text_metrics: Res<TextMetrics>,
    time: Res<Time>,
    mut atlas: Option<ResMut<crate::text::msdf::MsdfAtlas>>,
    mut font_data_map: ResMut<FontDataMap>,
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
        entity, mut block_scene, computed, mut node, rich, border, vis, effects,
        mut msdf_glyphs, mut render_method,
    ) in block_cells.iter_mut()
    {
        // Skip hidden blocks
        if *vis == Visibility::Hidden {
            continue;
        }

        let width = computed.size().x;
        if width <= 0.0 {
            continue;
        }

        // Check if rebuild is needed
        let version_changed = block_scene.content_version != block_scene.last_built_version;
        let width_changed = (block_scene.built_width - width).abs() > 1.0;
        let is_rainbow = effects.is_some_and(|e| e.rainbow);
        let has_animation = border.is_some_and(|b| b.animation != BorderAnimation::None);
        let needs_rebuild = version_changed || width_changed || is_rainbow || has_animation;

        if !needs_rebuild {
            continue;
        }

        // Font required for text rendering
        let Some(font) = font else {
            continue;
        };

        // Compute border padding offsets
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
        let max_advance = if content_width > 0.0 {
            Some(content_width)
        } else {
            None
        };

        let mut scene = vello::Scene::new();
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
            font: font_handles.mono.clone(),
            brush: text_brush.clone(),
            font_size: text_metrics.cell_font_size,
            font_axes: bevy_vello::integrations::text::VelloFontAxes {
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

                // MSDF renders text; Vello scene only gets borders
                if let Some(ref mut atlas) = atlas {
                    for line in layout.lines() {
                        for item in line.items() {
                            if let bevy_vello::parley::PositionedLayoutItem::GlyphRun(gr) = item {
                                font_data_map.register(gr.run().font());
                            }
                        }
                    }
                    let glyphs = collect_msdf_glyphs(
                        &layout, &span_brushes, &fallback_brush, text_offset, atlas,
                    );
                    msdf_glyphs.glyphs = glyphs;
                    msdf_glyphs.version = block_scene.scene_version.wrapping_add(1);
                    msdf_glyphs.rainbow = is_rainbow;
                    *render_method = BlockRenderMethod::Msdf;
                }
            }
            Some(RichContentKind::Sparkline(data)) => {
                *render_method = BlockRenderMethod::Vello;
                let w = content_width as f64;
                let h = theme.sparkline_height as f64;
                content_height = theme.sparkline_height;
                if w > 0.0 && h > 0.0 {
                    let paths = build_sparkline_paths(data, w, h, 4.0);
                    let mut sub_scene = vello::Scene::new();
                    render_sparkline_scene(&mut sub_scene, &paths, &sparkline_colors);
                    scene.append(&sub_scene, Some(Affine::translate(text_offset)));
                }
            }
            Some(RichContentKind::Svg {
                scene: svg_scene,
                width: svg_w,
                height: svg_h,
                ..
            }) => {
                *render_method = BlockRenderMethod::Vello;
                if *svg_w <= 0.0 {
                    content_height = 0.0;
                } else {
                    let w_scale = content_width / svg_w;
                    let h_scale = SVG_MAX_HEIGHT / svg_h;
                    let scale = w_scale.min(h_scale) as f64;
                    let scaled_h = *svg_h as f64 * scale;
                    content_height = scaled_h as f32;

                    let clip_rect = vello::kurbo::Rect::new(
                        pad_left as f64,
                        pad_top as f64,
                        (pad_left + content_width) as f64,
                        (pad_top + content_height) as f64,
                    );
                    scene.push_layer(
                        Fill::NonZero,
                        vello::peniko::Mix::Normal,
                        1.0,
                        Affine::IDENTITY,
                        &clip_rect,
                    );
                    let transform =
                        Affine::translate(text_offset) * Affine::scale(scale);
                    scene.append(svg_scene, Some(transform));
                    scene.pop_layer();
                }
            }
            Some(RichContentKind::Abc { tune, .. }) => {
                *render_method = BlockRenderMethod::Vello;
                let default_opts = kaijutsu_abc::engrave::EngravingOptions::default();
                let elements =
                    kaijutsu_abc::engrave::layout::engrave(tune, &default_opts);

                let notation_brush = bevy_color_to_brush(theme.block_assistant);
                let (abc_scene, intrinsic_w, intrinsic_h) =
                    crate::text::abc::render_engraving_to_scene(
                        &elements,
                        default_opts.margin,
                        &notation_brush,
                        Some(font),
                        &text_metrics,
                    );

                let w_scale = content_width as f64 / intrinsic_w;
                let h_scale = SVG_MAX_HEIGHT as f64 / intrinsic_h;
                let scale = w_scale.min(h_scale);
                content_height = (intrinsic_h * scale) as f32;

                let clip_rect = vello::kurbo::Rect::new(
                    pad_left as f64,
                    pad_top as f64,
                    (pad_left + content_width) as f64,
                    (pad_top + content_height) as f64,
                );
                scene.push_layer(
                    Fill::NonZero,
                    vello::peniko::Mix::Normal,
                    1.0,
                    Affine::IDENTITY,
                    &clip_rect,
                );
                scene.append(
                    &abc_scene,
                    Some(Affine::translate(text_offset) * Affine::scale(scale)),
                );
                scene.pop_layer();
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
                            if let bevy_vello::parley::PositionedLayoutItem::GlyphRun(gr) = item {
                                font_data_map.register(gr.run().font());
                            }
                        }
                    }
                    let glyphs = collect_msdf_glyphs(
                        &parley_layout, &span_brushes, &fallback_brush, text_offset, atlas,
                    );
                    msdf_glyphs.glyphs = glyphs;
                    msdf_glyphs.version = block_scene.scene_version.wrapping_add(1);
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
                            if let bevy_vello::parley::PositionedLayoutItem::GlyphRun(gr) = item {
                                font_data_map.register(gr.run().font());
                            }
                        }
                    }
                    let glyphs = collect_msdf_glyphs(
                        &layout, &[], &text_brush, text_offset, atlas,
                    );
                    msdf_glyphs.glyphs = glyphs;
                    msdf_glyphs.version = block_scene.scene_version.wrapping_add(1);
                    msdf_glyphs.rainbow = is_rainbow;
                    *render_method = BlockRenderMethod::Msdf;
                }
            }
        }

        let total_height = content_height + pad_top + pad_bottom;

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
                    font: font_handles.mono.clone(),
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

                    // Position: centered vertically on the top content boundary (y = pad_top)
                    let label_x = label_inset + label_pad;
                    let label_y = (pad_top - label_h).max(0.0) * 0.5;
                    let label_offset = (label_x as f64, label_y as f64);

                    for line in label_layout.lines() {
                        for item in line.items() {
                            if let bevy_vello::parley::PositionedLayoutItem::GlyphRun(gr) = item {
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

                    // Position: right-aligned, centered on bottom content boundary
                    let label_x = (width - label_inset - label_pad - label_w).max(0.0);
                    let label_y = (total_height - pad_bottom * 0.5 - label_h * 0.5).max(0.0);
                    let label_offset = (label_x as f64, label_y as f64);

                    for line in label_layout.lines() {
                        for item in line.items() {
                            if let bevy_vello::parley::PositionedLayoutItem::GlyphRun(gr) = item {
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

        // Set explicit height on the node
        let target_height = Val::Px(total_height);
        if node.height != target_height {
            node.height = target_height;
        }

        block_scene.scene = scene;
        block_scene.built_width = width;
        block_scene.built_height = total_height;
        block_scene.last_built_version = block_scene.content_version;
        block_scene.scene_version = block_scene.scene_version.wrapping_add(1);
    }
}

/// Build vello scenes for role group border entities.
pub fn build_role_group_scenes(
    mut role_borders: Query<
        (&RoleGroupBorder, &mut BlockScene, &ComputedNode),
        Changed<ComputedNode>,
    >,
    fonts: Res<Assets<VelloFont>>,
    font_handles: Res<FontHandles>,
    theme: Res<Theme>,
) {
    let font = fonts.get(&font_handles.mono);

    for (border, mut block_scene, computed) in role_borders.iter_mut() {
        let size = computed.size();
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

        let mut scene = vello::Scene::new();
        let height = size.y.max(20.0);
        fieldset::build_role_group_line(
            &mut scene,
            size.x as f64,
            height as f64,
            label,
            color,
            font,
        );

        block_scene.scene = scene;
        block_scene.built_width = size.x;
        block_scene.built_height = height;
        block_scene.scene_version = block_scene.scene_version.wrapping_add(1);
    }
}

/// Resize block textures to match physical pixel dimensions.
///
/// Runs after build_block_scenes so built_width/built_height are up to date.
/// Block cells update their `BlockFxMaterial` texture binding.
/// Role group borders update their `ImageNode` directly.
pub fn resize_block_textures(
    mut block_query: Query<
        (&BlockScene, &mut BlockTexture, &MaterialNode<BlockFxMaterial>, &mut ImageNode),
        (
            Or<(With<BlockCell>, With<crate::view::components::MsdfOverlayText>)>,
            Without<RoleGroupBorder>,
        ),
    >,
    mut role_query: Query<
        (&BlockScene, &mut BlockTexture, &mut ImageNode),
        (With<RoleGroupBorder>, Without<BlockCell>),
    >,
    text_metrics: Res<TextMetrics>,
    gpu_limits: Res<GpuTextureLimits>,
    mut images: ResMut<Assets<Image>>,
    mut fx_materials: ResMut<Assets<BlockFxMaterial>>,
) {
    let scale = text_metrics.scale_factor;
    let max_dim = gpu_limits.max_texture_dim;

    // Block cells: update material + ImageNode texture bindings.
    // ImageNode ensures GpuImage is prepared by Bevy's RenderAssetPlugin.
    // MaterialNode's shader reads from the same texture for post-processing.
    for (scene, mut texture, mat_node, mut image_node) in block_query.iter_mut() {
        if scene.built_width <= 0.0 || scene.built_height <= 0.0 {
            continue;
        }

        let target_w = (scene.built_width * scale).ceil() as u32;
        let target_h = (scene.built_height * scale).ceil() as u32;
        let target_w = target_w.clamp(1, max_dim);
        let target_h = target_h.clamp(1, max_dim);

        if texture.width != target_w || texture.height != target_h {
            let new_handle = create_block_texture(&mut images, target_w, target_h);
            if let Some(mat) = fx_materials.get_mut(&mat_node.0) {
                mat.texture = new_handle.clone();
            }
            image_node.image = new_handle.clone();
            texture.image = new_handle;
            texture.width = target_w;
            texture.height = target_h;
        }
    }

    // Role group borders: update ImageNode directly (no shader effects)
    for (scene, mut texture, mut image_node) in role_query.iter_mut() {
        if scene.built_width <= 0.0 || scene.built_height <= 0.0 {
            continue;
        }

        let target_w = (scene.built_width * scale).ceil() as u32;
        let target_h = (scene.built_height * scale).ceil() as u32;
        let target_w = target_w.clamp(1, max_dim);
        let target_h = target_h.clamp(1, max_dim);

        if texture.width != target_w || texture.height != target_h {
            let new_handle = create_block_texture(&mut images, target_w, target_h);
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

/// Extract dirty block scenes from the main world.
///
/// Runs in ExtractSchedule. Compares scene_version against last rendered
/// version to determine which scenes need GPU re-rendering.
pub fn extract_block_scenes(
    mut extracted: ResMut<ExtractedBlockScenes>,
    query: Extract<Query<(
        &BlockScene,
        &BlockTexture,
        Option<&BlockRenderMethod>,
    )>>,
) {
    extracted.items.clear();

    for (scene, texture, render_method) in query.iter() {
        if scene.scene_version == 0 {
            continue;
        }

        // MSDF blocks skip Vello entirely — text is rendered by the MSDF pass.
        // Borders will be replaced by shader-drawn borders later; borderless for now.
        if render_method == Some(&BlockRenderMethod::Msdf) {
            continue;
        }

        let asset_id = texture.image.id();
        let last = extracted.last_rendered.get(&asset_id).copied().unwrap_or(0);
        if scene.scene_version > last {
            extracted.items.push(ExtractedBlockSceneItem {
                scene: scene.scene.clone(),
                image_handle: texture.image.clone(),
                width: texture.width,
                height: texture.height,
                built_width: scene.built_width,
                built_height: scene.built_height,
                version: scene.scene_version,
            });
        }
    }
}

/// Render extracted block scenes to their per-block textures.
///
/// Runs in the Render schedule. Locks VelloRenderer once and renders all
/// dirty blocks, then unlocks.
pub fn render_block_textures(
    mut extracted: ResMut<ExtractedBlockScenes>,
    renderer: Res<bevy_vello::render::VelloRenderer>,
    device: Res<RenderDevice>,
    queue: Res<RenderQueue>,
    gpu_images: Res<RenderAssets<GpuImage>>,
    render_settings: Res<bevy_vello::render::VelloRenderSettings>,
) {
    if extracted.items.is_empty() {
        return;
    }

    let Ok(mut vello_renderer) = renderer.lock() else {
        warn!("Failed to lock VelloRenderer for block texture rendering");
        return;
    };

    let items: Vec<_> = extracted.items.drain(..).collect();

    for item in items {
        let Some(gpu_image) = gpu_images.get(&item.image_handle) else {
            // GpuImage not ready yet — don't update last_rendered so
            // next frame's extract will re-include this scene.
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

        // Scale the Vello scene to match physical texture dimensions.
        // This handles both:
        // - High-DPI (texture is larger than logical scene due to scale_factor)
        // - max_dim clamping (texture is smaller than logical scene)
        // Without scaling, Vello content (borders, SVG) would be misaligned
        // with MSDF text which renders in NDC and inherently fills the texture.
        let sx = item.width as f64 / item.built_width.max(1.0) as f64;
        let sy = item.height as f64 / item.built_height.max(1.0) as f64;
        let needs_scale = (sx - 1.0).abs() > 0.001 || (sy - 1.0).abs() > 0.001;

        let fitted_scene;
        let scene_to_render = if needs_scale {
            fitted_scene = {
                let mut s = vello::Scene::new();
                s.append(
                    &item.scene,
                    Some(Affine::scale_non_uniform(sx, sy)),
                );
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
            warn!("Block texture render failed: {e}");
            continue;
        }

        // Track successful render
        extracted
            .last_rendered
            .insert(item.image_handle.id(), item.version);
    }
}

/// Render MSDF text glyphs to per-block textures.
///
/// Runs after Vello rendering so borders are already present in the texture.
/// MSDF text is composited on top with premultiplied alpha blending.
pub fn render_msdf_block_textures(
    msdf_renderer: Option<Res<MsdfBlockRenderer>>,
    msdf_atlas: Res<ExtractedMsdfAtlas>,
    mut msdf_data: ResMut<ExtractedMsdfBlockData>,
    render_params: Res<ExtractedMsdfRenderParams>,
    device: Res<RenderDevice>,
    queue: Res<RenderQueue>,
    gpu_images: Res<RenderAssets<GpuImage>>,
    pipeline_cache: Res<PipelineCache>,
) {
    let Some(msdf_renderer) = msdf_renderer else {
        return;
    };

    let items: Vec<_> = msdf_data.items.drain(..).collect();

    for item in items {
        if item.glyphs.is_empty() {
            continue;
        }

        let vertices = MsdfBlockRenderer::build_vertices(
            &item.glyphs,
            &msdf_atlas,
            item.built_width,
            item.built_height,
            item.rainbow,
        );

        if vertices.is_empty() {
            continue;
        }

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

        // Clear if no Vello content (no border); composite if Vello drew borders first
        let clear = !item.has_vello_content;
        let rendered = msdf_renderer.render_to_texture(
            &device,
            &queue,
            &pipeline_cache,
            &gpu_images,
            &msdf_atlas.texture,
            &item.image_handle,
            &vertices,
            &uniforms,
            clear,
        );

        if rendered {
            msdf_data
                .last_rendered
                .insert(item.image_handle.id(), item.version);
        } else {
            let pipe_ok = pipeline_cache
                .get_render_pipeline(msdf_renderer.pipeline)
                .is_some();
            let pipe_state = pipeline_cache
                .get_render_pipeline_state(msdf_renderer.pipeline);
            let target_ok = gpu_images.get(&item.image_handle).is_some();
            let atlas_ok = gpu_images.get(&msdf_atlas.texture).is_some();
            warn!(
                "MSDF render skipped: {}x{} glyphs={} verts={} pipeline={} ({:?}) target_gpu={} atlas_gpu={}",
                item.width, item.height,
                item.glyphs.len(), vertices.len(),
                pipe_ok, pipe_state,
                target_ok, atlas_ok,
            );
        }
    }
}

// ============================================================================
// MSDF EXTRACTION
// ============================================================================

/// Extracted MSDF block data for the render world.
struct ExtractedMsdfBlockItem {
    glyphs: Vec<crate::text::msdf::PositionedGlyph>,
    image_handle: Handle<Image>,
    width: u32,
    height: u32,
    built_width: f32,
    built_height: f32,
    version: u64,
    rainbow: bool,
    /// Whether Vello rendered borders first (false = MSDF must clear).
    has_vello_content: bool,
}

/// Resource holding extracted MSDF block data.
#[derive(Resource, Default)]
pub struct ExtractedMsdfBlockData {
    items: Vec<ExtractedMsdfBlockItem>,
    last_rendered: HashMap<AssetId<Image>, u64>,
}

/// Extracted MSDF rendering parameters (from Theme + Time).
#[derive(Resource, Default, Clone)]
pub struct ExtractedMsdfRenderParams {
    pub hint_amount: f32,
    pub stem_darkening: f32,
    pub horz_scale: f32,
    pub vert_scale: f32,
    pub text_bias: f32,
    pub gamma_correction: f32,
    pub time: f32,
}

/// Extract MSDF atlas data to the render world.
fn extract_msdf_atlas(
    mut extracted: ResMut<ExtractedMsdfAtlas>,
    atlas: Extract<Option<Res<crate::text::msdf::MsdfAtlas>>>,
) {
    let Some(atlas) = atlas.as_ref() else {
        return;
    };

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
            &BlockRenderMethod,
            &BlockScene,
            &BlockTexture,
        )>,
    >,
) {
    extracted.items.clear();

    for (msdf_glyphs, render_method, scene, texture) in query.iter() {
        if msdf_glyphs.glyphs.is_empty() || msdf_glyphs.version == 0 {
            continue;
        }

        let asset_id = texture.image.id();
        let last = extracted.last_rendered.get(&asset_id).copied().unwrap_or(0);
        if msdf_glyphs.version > last {
            // Vello blocks have content rendered by the Vello pass — MSDF composites on top
            let has_vello = *render_method == BlockRenderMethod::Vello;
            extracted.items.push(ExtractedMsdfBlockItem {
                glyphs: msdf_glyphs.glyphs.clone(),
                image_handle: texture.image.clone(),
                width: texture.width,
                height: texture.height,
                built_width: scene.built_width,
                built_height: scene.built_height,
                version: msdf_glyphs.version,
                rainbow: msdf_glyphs.rainbow,
                has_vello_content: has_vello,
            });
        }
    }
}

/// Extract MSDF rendering params (theme + time) to the render world.
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
