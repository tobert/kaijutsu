//! Per-block Vello texture rendering.
//!
//! Each conversation block renders its own `vello::Scene` to a per-block texture,
//! displayed as a Bevy `ImageNode`. Bevy's UI system handles scroll, clip, z-order.
//! `BackgroundColor` works again. No more coordinate space mismatches.

use std::collections::HashMap;

use bevy::prelude::*;
use bevy::render::{
    Extract, ExtractSchedule, Render, RenderApp, RenderSystems,
    render_asset::RenderAssets,
    render_resource::{Extent3d, TextureDimension, TextureFormat, TextureUsages},
    renderer::{RenderDevice, RenderQueue},
    texture::GpuImage,
};
use bevy_vello::prelude::*;
use bevy_vello::vello;
use vello::kurbo::Affine;
use vello::peniko::Fill;

use crate::cell::block_border::{BlockBorderStyle, BorderAnimation};
use crate::cell::{BlockCell, RoleGroupBorder};
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
        // Main world: build scenes and resize textures in PostUpdate
        // after Taffy layout so ComputedNode.size() is available.
        app.add_systems(
            PostUpdate,
            (
                build_block_scenes.after(bevy::ui::UiSystems::Layout),
                build_role_group_scenes.after(bevy::ui::UiSystems::Layout),
                resize_block_textures
                    .after(build_block_scenes)
                    .after(build_role_group_scenes),
            ),
        );

        // Render world: extract and render
        let Some(render_app) = app.get_sub_app_mut(RenderApp) else {
            return;
        };

        render_app
            .init_resource::<ExtractedBlockScenes>()
            .add_systems(ExtractSchedule, extract_block_scenes)
            .add_systems(
                Render,
                render_block_textures
                    .in_set(RenderSystems::Render)
                    .run_if(|scenes: Res<ExtractedBlockScenes>| !scenes.items.is_empty()),
            );
    }
}

// ============================================================================
// TEXTURE HELPERS
// ============================================================================

/// Maximum texture dimension (pixels). Most GPUs support 16384, but some
/// older/mobile GPUs cap at 8192. Use the conservative limit.
const MAX_TEXTURE_DIM: u32 = 8192;

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
        | TextureUsages::STORAGE_BINDING;
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
    mut block_cells: Query<
        (
            &BlockCell,
            &mut BlockScene,
            &ComputedNode,
            &mut Node,
            Option<&RichContent>,
            Option<&BlockBorderStyle>,
            &Visibility,
            Option<&KjTextEffects>,
        ),
    >,
    fonts: Res<Assets<VelloFont>>,
    font_handles: Res<FontHandles>,
    theme: Res<Theme>,
    text_metrics: Res<TextMetrics>,
    time: Res<Time>,
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

    for (_block_cell, mut block_scene, computed, mut node, rich, border, vis, effects) in
        block_cells.iter_mut()
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
                crate::text::rich::render_layout_with_brushes(
                    &mut scene,
                    &layout,
                    &span_brushes,
                    &fallback_brush,
                    text_offset,
                );
            }
            Some(RichContentKind::Sparkline(data)) => {
                let w = content_width as f64;
                let h = theme.sparkline_height as f64;
                content_height = theme.sparkline_height;
                if w > 0.0 && h > 0.0 {
                    let paths = build_sparkline_paths(data, w, h, 4.0);
                    // Offset sparkline into content area
                    let mut sub_scene = vello::Scene::new();
                    render_sparkline_scene(&mut sub_scene, &paths, &sparkline_colors);
                    scene.append(&sub_scene, Some(Affine::translate(text_offset)));
                }
            }
            Some(RichContentKind::Svg {
                scene: svg_scene,
                width: svg_w,
                height: svg_h,
            }) => {
                if *svg_w <= 0.0 {
                    content_height = 0.0;
                } else {
                    // Scale to fit content width, then constrain to max height
                    let w_scale = (content_width / svg_w).min(1.0);
                    let h_scale = (SVG_MAX_HEIGHT / svg_h).min(1.0);
                    let scale = w_scale.min(h_scale) as f64;
                    let scaled_h = *svg_h as f64 * scale;
                    content_height = scaled_h as f32;

                    // Clip to content area and transform
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
                crate::text::rich::render_layout_with_brushes(
                    &mut scene,
                    &parley_layout,
                    &span_brushes,
                    &fallback_brush,
                    text_offset,
                );
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

                crate::text::rich::render_layout_with_brushes(
                    &mut scene,
                    &layout,
                    &[],
                    &text_brush,
                    text_offset,
                );
            }
        }

        let total_height = content_height + pad_top + pad_bottom;

        // Add border to the scene
        if let Some(border_style) = border {
            let t = if has_animation {
                time.elapsed_secs()
            } else {
                0.0
            };
            fieldset::build_fieldset_border(
                &mut scene,
                width as f64,
                total_height as f64,
                border_style,
                border_style.top_label.as_deref(),
                border_style.bottom_label.as_deref(),
                Some(font),
                t,
                theme.bg,
            );
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
        (&RoleGroupBorder, &mut BlockScene, &ComputedNode, &mut Node),
        Changed<ComputedNode>,
    >,
    fonts: Res<Assets<VelloFont>>,
    font_handles: Res<FontHandles>,
    theme: Res<Theme>,
) {
    let font = fonts.get(&font_handles.mono);

    for (border, mut block_scene, computed, _node) in role_borders.iter_mut() {
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
pub fn resize_block_textures(
    mut query: Query<(&BlockScene, &mut BlockTexture, &mut ImageNode)>,
    text_metrics: Res<TextMetrics>,
    mut images: ResMut<Assets<Image>>,
) {
    let scale = text_metrics.scale_factor;

    for (scene, mut texture, mut image_node) in query.iter_mut() {
        if scene.built_width <= 0.0 || scene.built_height <= 0.0 {
            continue;
        }

        let target_w = (scene.built_width * scale).ceil() as u32;
        let target_h = (scene.built_height * scale).ceil() as u32;
        let target_w = target_w.clamp(1, MAX_TEXTURE_DIM);
        let target_h = target_h.clamp(1, MAX_TEXTURE_DIM);

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
    query: Extract<Query<(&BlockScene, &BlockTexture)>>,
) {
    extracted.items.clear();

    for (scene, texture) in query.iter() {
        if scene.scene_version == 0 {
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

        if let Err(e) = vello_renderer.render_to_texture(
            device.wgpu_device(),
            &queue,
            &item.scene,
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
