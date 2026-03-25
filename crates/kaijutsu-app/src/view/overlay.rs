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
#[derive(Component, Reflect, Clone)]
#[reflect(Component)]
pub struct OverlayStyle {
    pub bg_color: Color,
    pub border_color: Color,
    pub border_thickness: f32,
    pub corner_radius: f32,
    pub glow_radius: f32,
    pub glow_intensity: f32,
    pub animation: OverlayAnimation,
}

impl Default for OverlayStyle {
    fn default() -> Self {
        Self {
            bg_color: Color::srgba(0.102, 0.106, 0.149, 0.95),
            border_color: Color::srgb(0.478, 0.635, 0.969),
            border_thickness: 2.0,
            corner_radius: 8.0,
            glow_radius: 6.0,
            glow_intensity: 0.25,
            animation: OverlayAnimation::Breathe,
        }
    }
}

/// Animation style for the overlay border.
#[derive(Default, Clone, Copy, PartialEq, Eq, Reflect, Debug)]
pub enum OverlayAnimation {
    #[default]
    None,
    /// Subtle alpha oscillation
    Breathe,
    /// Running light around border
    Chase,
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

/// Spawn the InputOverlay entity with two children:
/// 1. MSDF text surface (BlockScene + BlockFxMaterial for shader borders)
/// 2. Cursor overlay (UiVelloScene for beam drawing)
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
        border_color: theme.compose_palette_border,
        border_thickness: 2.0,
        corner_radius: 8.0,
        glow_radius: theme.compose_palette_glow_radius,
        glow_intensity: theme.compose_palette_glow_intensity,
        animation: OverlayAnimation::Breathe,
    };

    let material_handle = fx_materials.add(BlockFxMaterial::default());

    let parent_node = Node {
        position_type: PositionType::Absolute,
        top: Val::Percent(15.0),
        left: Val::Percent(20.0),
        right: Val::Percent(20.0),
        min_height: Val::Px(48.0),
        max_width: Val::Px(800.0),
        ..default()
    };

    commands
        .spawn((InputOverlayMarker, InputOverlay::default(), style, parent_node, Visibility::Hidden))
        .insert(ZIndex(crate::constants::ZLayer::MODAL))
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

/// Update summon animation state when focus changes.
pub fn update_summon_animation(focus: Res<FocusArea>, mut summon: ResMut<OverlaySummonState>) {
    let should_show = matches!(*focus, FocusArea::Compose);
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

            // Check if rebuild needed (text changed or width changed)
            let display = overlay.display_text();
            let width_changed = (block_scene.built_width - width).abs() > 1.0;
            let text_changed = block_scene.text != display;

            if !text_changed && !width_changed {
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
            let layout = font.layout(&display, &style, VelloTextAlign::Left, max_advance);
            let content_height = layout.height();

            let text_offset = (pad.left as f64, pad.top as f64);

            // Collect MSDF glyphs
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

            // Set scene dimensions (content + padding)
            let total_height = content_height + pad.top + pad.bottom;
            block_scene.built_width = width;
            block_scene.built_height = total_height;
            block_scene.text = display.clone();
            block_scene.color = text_color;
            block_scene.content_version = block_scene.content_version.wrapping_add(1);
            block_scene.last_built_version = block_scene.content_version;
            block_scene.scene_version = block_scene.scene_version.wrapping_add(1);

            // Set explicit height on the node
            node.height = Val::Px(total_height);

            // Compute cursor geometry from Parley layout
            let cursor_byte_offset = overlay.display_cursor_offset();
            let cursor = parley::editing::Cursor::from_byte_index(
                &layout,
                cursor_byte_offset,
                parley::layout::Affinity::Upstream,
            );
            let geom = cursor.geometry(&layout, 2.0);
            cursor_geom.x = text_offset.0 + geom.x0;
            cursor_geom.y = text_offset.1 + geom.y0;
            cursor_geom.height = geom.y1 - geom.y0;
        }
    }
}

// ============================================================================
// THEME SYNC
// ============================================================================

/// Sync OverlayStyle + BlockBorderStyle from theme when theme changes.
pub fn sync_overlay_style_to_theme(
    theme: Res<Theme>,
    mut overlay_query: Query<(&mut OverlayStyle, &Children), With<InputOverlayMarker>>,
    mut border_query: Query<&mut BlockBorderStyle, With<MsdfOverlayText>>,
) {
    if !theme.is_changed() {
        return;
    }
    for (mut style, children) in overlay_query.iter_mut() {
        style.bg_color = theme.compose_bg.with_alpha(0.95);
        style.border_color = theme.compose_palette_border;
        style.glow_radius = theme.compose_palette_glow_radius;
        style.glow_intensity = theme.compose_palette_glow_intensity;

        // Sync border style on MSDF child
        for child in children.iter() {
            if let Ok(mut border) = border_query.get_mut(child) {
                border.color = theme.compose_palette_border;
            }
        }
    }
}
