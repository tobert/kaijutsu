//! Input overlay — MSDF-rendered floating command palette.
//!
//! The InputOverlay is the sole compose surface, rendered as a floating
//! command palette (rofi/dmenu style) with:
//! - Centered positioning (~60% width, near top-third)
//! - MSDF text rendering (GPU-native hinting, directional AA)
//! - Shader-rendered borders + cursor beam via BlockFxMaterial
//! - Summon animation (fade-in via visibility)

use bevy::prelude::*;
use bevy::ui::ComputedNode;
use bevy_vello::parley;
use bevy_vello::prelude::{VelloFont, VelloTextAlign, VelloTextStyle};

use crate::cell::block_border::{BlockBorderStyle, BorderAnimation, BorderKind, BorderPadding};
use crate::cell::{InputOverlay, InputOverlayMarker, MsdfOverlayText};
use crate::input::FocusArea;
use crate::shaders::BlockFxMaterial;
use crate::text::msdf::{BlockRenderMethod, FontDataMap, MsdfBlockGlyphs, collect_msdf_glyphs};
use crate::text::{FontHandles, TextMetrics, bevy_color_to_brush};
use crate::ui::theme::Theme;
use crate::view::block_render::{BlockScene, BlockTexture};
use crate::view::components::OverlayCursorGeometry;

// ============================================================================
// COMPONENTS
// ============================================================================

/// Style for the command palette overlay.
///
/// Only stores values consumed directly by the parent entity's Bevy UI
/// components. Border/glow styling lives on the child's `BlockBorderStyle`
/// and is read by the shader system from `Theme` directly.
#[derive(Component, Reflect, Clone)]
#[reflect(Component)]
pub struct OverlayStyle {
    pub bg_color: Color,
    pub corner_radius: f32,
}

impl Default for OverlayStyle {
    fn default() -> Self {
        Self {
            bg_color: Color::srgba(0.102, 0.106, 0.149, 0.95),
            corner_radius: 8.0,
        }
    }
}

/// Summon animation state.
#[derive(Resource, Default)]
pub struct OverlaySummonState {
    /// Animation progress: 0.0 = hidden, 1.0 = fully visible
    pub progress: f32,
    /// Target visibility state
    pub target_visible: bool,
    /// Whether animation is in progress
    pub animating: bool,
}

// ============================================================================
// SPAWN SYSTEM
// ============================================================================

/// Spawn the InputOverlay entity with one MSDF text child.
///
/// The child holds the `BlockScene` + `BlockFxMaterial` for MSDF text,
/// shader-drawn borders, and cursor beam rendering.
pub fn spawn_input_overlay(
    mut commands: Commands,
    existing: Query<Entity, With<InputOverlayMarker>>,
    theme: Res<Theme>,
    mut fx_materials: ResMut<Assets<BlockFxMaterial>>,
) {
    if !existing.is_empty() {
        return;
    }

    let style = OverlayStyle {
        bg_color: theme.compose_bg.with_alpha(0.95),
        corner_radius: 8.0,
    };

    let material_handle = fx_materials.add(BlockFxMaterial::default());

    let parent_node = Node {
        position_type: PositionType::Absolute,
        top: Val::Percent(15.0),
        left: Val::Percent(20.0),
        right: Val::Percent(20.0),
        min_height: Val::Px(48.0),
        max_width: Val::Px(800.0),
        overflow: Overflow::clip(),
        border_radius: BorderRadius::all(Val::Px(style.corner_radius)),
        ..default()
    };

    commands
        .spawn((InputOverlayMarker, InputOverlay::default(), style.clone(), parent_node, Visibility::Hidden))
        .insert(ZIndex(crate::constants::ZLayer::MODAL))
        .insert(BackgroundColor(style.bg_color))
        .with_children(|parent| {
            // Child 1: MSDF text surface with shader borders
            parent.spawn((
                MsdfOverlayText,
                BlockScene::default(),
                BlockTexture {
                    image: Handle::default(),
                    width: 1,
                    height: 1,
                },
                MsdfBlockGlyphs::default(),
                BlockRenderMethod::Msdf,
                ImageNode::default(),
                MaterialNode(material_handle),
                BlockBorderStyle {
                    kind: BorderKind::Full,
                    color: theme.compose_palette_border,
                    thickness: 2.0,
                    corner_radius: 8.0,
                    padding: BorderPadding {
                        top: 16.0,
                        bottom: 16.0,
                        left: 16.0,
                        right: 16.0,
                    },
                    animation: BorderAnimation::Breathe,
                    top_label: None,
                    bottom_label: None,
                },
                OverlayCursorGeometry::default(),
                Node {
                    width: Val::Percent(100.0),
                    ..default()
                },
            ));

        });

    info!("Spawned InputOverlay entity (MSDF child)");
}

// ============================================================================
// ANIMATION SYSTEMS
// ============================================================================

/// Update summon animation state when focus or surface changes.
///
/// Only shows the floating chat overlay when `ActiveSurface::Chat` + `FocusArea::Compose`.
/// Shell surface uses the bottom dock instead.
pub fn update_summon_animation(
    focus: Res<FocusArea>,
    surface: Res<crate::input::focus::ActiveSurface>,
    mut summon: ResMut<OverlaySummonState>,
) {
    let should_show = matches!(*focus, FocusArea::Compose) && !surface.is_shell();
    if summon.target_visible != should_show {
        summon.target_visible = should_show;
        summon.animating = true;
    }
}

/// Interpolate summon progress (fade in/out).
pub fn animate_summon(time: Res<Time>, mut summon: ResMut<OverlaySummonState>) {
    if !summon.animating {
        return;
    }

    let target = if summon.target_visible { 1.0 } else { 0.0 };
    let speed = 8.0; // ~125ms animation
    let delta = time.delta_secs() * speed;

    if summon.progress < target {
        summon.progress = (summon.progress + delta).min(target);
    } else {
        summon.progress = (summon.progress - delta).max(target);
    }

    if (summon.progress - target).abs() < 0.001 {
        summon.progress = target;
        summon.animating = false;
    }
}

/// Show/hide the InputOverlay entity based on summon progress.
pub fn sync_overlay_visibility(
    summon: Res<OverlaySummonState>,
    mut overlay_query: Query<&mut Visibility, With<InputOverlayMarker>>,
) {
    if !summon.is_changed() {
        return;
    }
    for mut vis in overlay_query.iter_mut() {
        *vis = if summon.progress > 0.0 {
            Visibility::Inherited
        } else {
            Visibility::Hidden
        };
    }
}

// ============================================================================
// MSDF GLYPH BUILDING (PostUpdate, after Layout)
// ============================================================================

/// Build MSDF glyphs for the overlay text surface.
///
/// Runs in PostUpdate after UiSystems::Layout so ComputedNode is available.
/// Replaces the old sync_input_overlay_buffer + sync_overlay_max_advance.
pub fn build_overlay_glyphs(
    parents: Query<(&InputOverlay, &Children), With<InputOverlayMarker>>,
    mut msdf_children: Query<
        (
            &mut BlockScene,
            &mut MsdfBlockGlyphs,
            &BlockBorderStyle,
            &ComputedNode,
            &mut Node,
            &mut OverlayCursorGeometry,
        ),
        With<MsdfOverlayText>,
    >,
    fonts: Res<Assets<VelloFont>>,
    font_handles: Res<FontHandles>,
    theme: Res<Theme>,
    text_metrics: Res<TextMetrics>,
    mut atlas: Option<ResMut<crate::text::msdf::MsdfAtlas>>,
    mut font_data_map: ResMut<FontDataMap>,
) {
    let Some(font) = fonts.get(&font_handles.mono) else {
        return;
    };

    for (overlay, children) in parents.iter() {
        for child in children.iter() {
            let Ok((
                mut block_scene,
                mut msdf_glyphs,
                border_style,
                computed,
                mut node,
                mut cursor_geom,
            )) = msdf_children.get_mut(child)
            else {
                continue;
            };

            let width = computed.size().x;
            if width <= 0.0 {
                continue;
            }

            // Check what changed: text/width requires full relayout,
            // cursor-only requires just geometry recomputation.
            let display = overlay.display_text();
            let cursor_byte_offset = overlay.display_cursor_offset();
            let width_changed = (block_scene.built_width - width).abs() > 1.0;
            let text_changed = block_scene.text != display;
            let cursor_changed = cursor_geom.last_cursor_offset != cursor_byte_offset;

            if !text_changed && !width_changed && !cursor_changed {
                continue;
            }

            // Determine text color
            let text_color = if overlay.is_empty() {
                theme.fg_dim
            } else if overlay.is_shell() {
                let validation = crate::kaish::validate(&overlay.text);
                if !validation.valid && !validation.incomplete {
                    theme.block_tool_error
                } else {
                    theme.block_user
                }
            } else {
                theme.block_user
            };

            let text_brush = bevy_color_to_brush(text_color);

            // Compute content area (inside border padding)
            let pad = &border_style.padding;
            let content_width = (width - pad.left - pad.right).max(0.0);
            let max_advance = if content_width > 0.0 {
                Some(content_width)
            } else {
                None
            };

            // Build text style
            let style = VelloTextStyle {
                font: font_handles.mono.clone(),
                brush: text_brush,
                font_size: text_metrics.cell_font_size,
                font_axes: bevy_vello::integrations::text::VelloFontAxes {
                    weight: Some(200.0),
                    ..default()
                },
                ..default()
            };

            // Run Parley layout
            let layout = font.layout(display, &style, VelloTextAlign::Left, max_advance);
            let content_height = layout.height();

            let text_offset = (pad.left as f64, pad.top as f64);

            // Only rebuild glyphs when text or width changed (not cursor-only)
            if text_changed || width_changed {
                if let Some(ref mut atlas) = atlas {
                    for line in layout.lines() {
                        for item in line.items() {
                            if let bevy_vello::parley::PositionedLayoutItem::GlyphRun(gr) = item {
                                font_data_map.register(gr.run().font());
                            }
                        }
                    }
                    let glyphs =
                        collect_msdf_glyphs(&layout, &[], &style.brush, text_offset, atlas);
                    msdf_glyphs.glyphs = glyphs;
                    msdf_glyphs.version = msdf_glyphs.version.wrapping_add(1);
                    msdf_glyphs.rainbow = false;
                }

                // Set scene dimensions (content + padding).
                // Round to physical pixel boundary — see block_render.rs comment.
                let scale = text_metrics.scale_factor;
                let total_height = ((content_height + pad.top + pad.bottom) * scale).round() / scale;
                block_scene.built_width = width;
                block_scene.built_height = total_height;
                block_scene.text = display.to_string();
                block_scene.color = text_color;
                block_scene.content_version = block_scene.content_version.wrapping_add(1);
                block_scene.last_built_version = block_scene.content_version;
                block_scene.scene_version = block_scene.scene_version.wrapping_add(1);

                // Set explicit height on the node
                node.height = Val::Px(total_height);
            }

            // Always recompute cursor geometry when anything changed
            let cursor = parley::editing::Cursor::from_byte_index(
                &layout,
                cursor_byte_offset,
                parley::layout::Affinity::Upstream,
            );
            let geom = cursor.geometry(&layout, 2.0);
            cursor_geom.x = text_offset.0 + geom.x0;
            cursor_geom.y = text_offset.1 + geom.y0;
            cursor_geom.height = geom.y1 - geom.y0;
            cursor_geom.last_cursor_offset = cursor_byte_offset;
        }
    }
}

// ============================================================================
// THEME SYNC
// ============================================================================

/// Sync OverlayStyle + BlockBorderStyle from theme when theme changes.
pub fn sync_overlay_style_to_theme(
    theme: Res<Theme>,
    mut overlay_query: Query<
        (&mut OverlayStyle, &mut BackgroundColor, &Children),
        With<InputOverlayMarker>,
    >,
    mut border_query: Query<&mut BlockBorderStyle, With<MsdfOverlayText>>,
) {
    if !theme.is_changed() {
        return;
    }
    for (mut style, mut bg, children) in overlay_query.iter_mut() {
        let new_bg = theme.compose_bg.with_alpha(0.95);
        style.bg_color = new_bg;
        *bg = BackgroundColor(new_bg);

        // Sync border style on MSDF child
        for child in children.iter() {
            if let Ok(mut border) = border_query.get_mut(child) {
                border.color = theme.compose_palette_border;
            }
        }
    }
}
