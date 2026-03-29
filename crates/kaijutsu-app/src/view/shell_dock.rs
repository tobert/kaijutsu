//! Shell dock — bottom-anchored kaish input surface.
//!
//! Spatially separated from the floating chat overlay. Appears when
//! `ActiveSurface::Shell` + `FocusArea::Compose`, collapses otherwise.
//! Uses the same MSDF text + shader border rendering as the chat overlay.

use bevy::prelude::*;
use bevy::ui::ComputedNode;
use bevy_vello::prelude::{VelloFont, VelloTextAlign, VelloTextStyle};

use crate::cell::block_border::{BlockBorderStyle, BorderAnimation, BorderKind, BorderPadding};
use crate::cell::InputOverlay;
use crate::input::focus::ActiveSurface;
use crate::input::FocusArea;
use crate::shaders::BlockFxMaterial;
use crate::text::msdf::{BlockRenderMethod, FontDataMap, MsdfBlockGlyphs, collect_msdf_glyphs};
use crate::text::{FontHandles, TextMetrics, bevy_color_to_brush};
use crate::ui::theme::Theme;
use crate::view::block_render::{BlockScene, BlockTexture};
use crate::view::components::OverlayCursorGeometry;
use crate::view::overlay::OverlayStyle;

// ============================================================================
// COMPONENTS
// ============================================================================

/// Marker for the shell dock input entity.
#[derive(Component)]
pub struct ShellDockMarker;

/// Marker for the MSDF text surface child of the shell dock.
#[derive(Component)]
pub struct MsdfShellDockText;

/// Summon animation state for the shell dock.
#[derive(Resource, Default)]
pub struct ShellDockSummonState {
    pub progress: f32,
    pub target_visible: bool,
    pub animating: bool,
}

// ============================================================================
// SPAWN SYSTEM
// ============================================================================

/// Spawn the shell dock input entity as a child of TilingRoot,
/// inserted before the SouthDock.
pub fn spawn_shell_dock(
    mut commands: Commands,
    existing: Query<Entity, With<ShellDockMarker>>,
    theme: Res<Theme>,
    mut fx_materials: ResMut<Assets<BlockFxMaterial>>,
    tiling_root: Query<(Entity, &Children), With<crate::ui::tiling_reconciler::TilingRoot>>,
    south_dock: Query<Entity, With<crate::ui::dock::SouthDock>>,
) {
    if !existing.is_empty() {
        return;
    }

    let Ok((root, root_children)) = tiling_root.single() else {
        return;
    };

    let style = OverlayStyle {
        bg_color: theme.compose_bg.with_alpha(0.92),
        corner_radius: 0.0, // Dock-style: no rounded corners
    };

    let material_handle = fx_materials.add(BlockFxMaterial::default());

    // Starts collapsed (display: None, height: 0)
    let parent_node = Node {
        width: Val::Percent(100.0),
        min_height: Val::Px(0.0),
        display: Display::None,
        overflow: Overflow::clip(),
        border: UiRect::top(Val::Px(1.0)),
        ..default()
    };

    let mut shell_overlay = InputOverlay::default();
    shell_overlay.mode = crate::view::components::InputMode::Shell;

    let shell_entity = commands
        .spawn((
            ShellDockMarker,
            shell_overlay,
            style.clone(),
            parent_node,
            Visibility::Hidden,
        ))
        .insert(BackgroundColor(style.bg_color))
        .insert(BorderColor::all(theme.border))
        .with_children(|parent| {
            parent.spawn((
                MsdfShellDockText,
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
                    kind: BorderKind::TopAccent,
                    color: theme.compose_palette_border,
                    thickness: 2.0,
                    corner_radius: 0.0,
                    padding: BorderPadding {
                        top: 8.0,
                        bottom: 8.0,
                        left: 16.0,
                        right: 16.0,
                    },
                    animation: BorderAnimation::None,
                    top_label: None,
                    bottom_label: None,
                },
                OverlayCursorGeometry::default(),
                Node {
                    width: Val::Percent(100.0),
                    ..default()
                },
            ));
        })
        .id();

    // Insert before SouthDock. Find SouthDock's index in root children.
    if let Ok(south) = south_dock.single() {
        if let Some(idx) = root_children.iter().position(|c| c == south) {
            commands
                .entity(root)
                .insert_children(idx, &[shell_entity]);
        } else {
            // Fallback: just add as child (will be after SouthDock)
            commands.entity(root).add_child(shell_entity);
        }
    } else {
        commands.entity(root).add_child(shell_entity);
    }

    info!("Spawned ShellDock entity (MSDF child)");
}

// ============================================================================
// VISIBILITY / ANIMATION SYSTEMS
// ============================================================================

/// Update shell dock summon state when focus or surface changes.
pub fn update_shell_dock_summon(
    focus: Res<FocusArea>,
    surface: Res<ActiveSurface>,
    mut summon: ResMut<ShellDockSummonState>,
) {
    let should_show = matches!(*focus, FocusArea::Compose) && surface.is_shell();
    if summon.target_visible != should_show {
        summon.target_visible = should_show;
        summon.animating = true;
    }
}

/// Interpolate shell dock summon progress.
pub fn animate_shell_dock_summon(time: Res<Time>, mut summon: ResMut<ShellDockSummonState>) {
    if !summon.animating {
        return;
    }

    let target = if summon.target_visible { 1.0 } else { 0.0 };
    let speed = 10.0; // Slightly faster than chat overlay
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

/// Show/hide the shell dock entity + adjust display/height based on summon progress.
pub fn sync_shell_dock_visibility(
    summon: Res<ShellDockSummonState>,
    mut shell_query: Query<(&mut Visibility, &mut Node), With<ShellDockMarker>>,
) {
    if !summon.is_changed() {
        return;
    }
    for (mut vis, mut node) in shell_query.iter_mut() {
        if summon.progress > 0.0 {
            *vis = Visibility::Inherited;
            node.display = Display::Flex;
            node.min_height = Val::Px(40.0);
        } else {
            *vis = Visibility::Hidden;
            node.display = Display::None;
            node.min_height = Val::Px(0.0);
        }
    }
}

// ============================================================================
// MSDF GLYPH BUILDING (PostUpdate, after Layout)
// ============================================================================

/// Build MSDF glyphs for the shell dock text surface.
///
/// Mirrors `build_overlay_glyphs` from overlay.rs but targets ShellDockMarker
/// and uses a `$ ` prompt prefix instead of the mode ring.
pub fn build_shell_dock_glyphs(
    parents: Query<(&InputOverlay, &Children), With<ShellDockMarker>>,
    mut msdf_children: Query<
        (
            &mut BlockScene,
            &mut MsdfBlockGlyphs,
            &BlockBorderStyle,
            &ComputedNode,
            &mut Node,
            &mut OverlayCursorGeometry,
        ),
        With<MsdfShellDockText>,
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

            // Shell dock display: "$ " prefix + text
            let vim_prefix = match &overlay.vim_mode {
                Some(vim) => format!("{} ", vim),
                None => String::new(),
            };
            let display = if overlay.is_empty() {
                format!("{}$ ", vim_prefix)
            } else {
                format!("{}$ {}", vim_prefix, overlay.text)
            };

            let cursor_byte_offset = vim_prefix.len() + 2 + overlay.cursor; // "$ " = 2 bytes

            let width_changed = (block_scene.built_width - width).abs() > 1.0;
            let text_changed = block_scene.text != display;

            if !text_changed && !width_changed {
                continue;
            }

            // Determine text color — kaish syntax validation
            let text_color = if overlay.is_empty() {
                theme.fg_dim
            } else {
                let validation = crate::kaish::validate(&overlay.text);
                if !validation.valid && !validation.incomplete {
                    theme.block_tool_error
                } else {
                    theme.block_user
                }
            };

            let text_brush = bevy_color_to_brush(text_color);

            let pad = &border_style.padding;
            let content_width = (width - pad.left - pad.right).max(0.0);
            let max_advance = if content_width > 0.0 {
                Some(content_width)
            } else {
                None
            };

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

            let layout = font.layout(&display, &style, VelloTextAlign::Left, max_advance);
            let content_height = layout.height();

            let text_offset = (pad.left as f64, pad.top as f64);

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

            let total_height = content_height + pad.top + pad.bottom;
            block_scene.built_width = width;
            block_scene.built_height = total_height;
            block_scene.text = display;
            block_scene.color = text_color;
            block_scene.content_version = block_scene.content_version.wrapping_add(1);
            block_scene.last_built_version = block_scene.content_version;
            block_scene.scene_version = block_scene.scene_version.wrapping_add(1);

            node.height = Val::Px(total_height);

            // Cursor geometry
            let cursor = bevy_vello::parley::editing::Cursor::from_byte_index(
                &layout,
                cursor_byte_offset,
                bevy_vello::parley::layout::Affinity::Upstream,
            );
            let geom = cursor.geometry(&layout, 2.0);
            cursor_geom.x = text_offset.0 + geom.x0;
            cursor_geom.y = text_offset.1 + geom.y0;
            cursor_geom.height = geom.y1 - geom.y0;
        }
    }
}

/// Sync shell dock style from theme when theme changes.
pub fn sync_shell_dock_style_to_theme(
    theme: Res<Theme>,
    mut dock_query: Query<
        (&mut OverlayStyle, &mut BackgroundColor, &mut BorderColor, &Children),
        With<ShellDockMarker>,
    >,
    mut border_query: Query<&mut BlockBorderStyle, With<MsdfShellDockText>>,
) {
    if !theme.is_changed() {
        return;
    }
    for (mut style, mut bg, mut border_color, children) in dock_query.iter_mut() {
        let new_bg = theme.compose_bg.with_alpha(0.92);
        style.bg_color = new_bg;
        *bg = BackgroundColor(new_bg);
        *border_color = BorderColor::all(theme.border);

        for child in children.iter() {
            if let Ok(mut border) = border_query.get_mut(child) {
                border.color = theme.compose_palette_border;
            }
        }
    }
}
