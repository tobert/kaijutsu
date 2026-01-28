//! Text rendering plugin for Bevy using MSDF.
//!
//! Sets up MSDF text rendering integrated with Bevy's render pipeline.

use bevy::prelude::*;
use bevy::render::{
    render_graph::{RenderGraphExt, ViewNodeRunner},
    ExtractSchedule, Render, RenderApp, RenderPlugin,
};
use bevy::ui::{ComputedNode, UiGlobalTransform, UiSystems};
use bevy::window::PrimaryWindow;

use super::msdf::{
    extract_msdf_texts, init_msdf_resources, prepare_msdf_texts, MsdfAtlas, MsdfGenerator,
    MsdfText, MsdfTextAreaConfig, MsdfTextBuffer, MsdfTextPipeline, MsdfTextRenderNode,
    MsdfUiText, UiTextPositionCache,
};
#[cfg(debug_assertions)]
use super::msdf::{DebugOverlayMode, MsdfDebugInfo, MsdfDebugOverlay};
use super::resources::*;

/// Marker component for debug HUD text.
#[cfg(debug_assertions)]
#[derive(Component)]
pub struct MsdfDebugHud;

/// Plugin that enables MSDF text rendering in Bevy.
pub struct TextRenderPlugin;

impl Plugin for TextRenderPlugin {
    fn build(&self, app: &mut App) {
        // Main world resources
        app.init_resource::<SharedFontSystem>()
            .init_resource::<TextResolution>()
            .init_resource::<TextMetrics>();

        #[cfg(debug_assertions)]
        app.init_resource::<MsdfDebugInfo>()
            .init_resource::<MsdfDebugOverlay>();

        app.add_systems(Update, (
                update_text_resolution,
                init_ui_text_buffers,
                update_ui_text_buffers,
                sync_ui_text_config_positions,
                request_atlas_glyphs,
                update_msdf_generator,
            ).chain());

        #[cfg(debug_assertions)]
        app.add_systems(Update, (
                debug_dump_atlas_on_f12,
                debug_toggle_overlay_f11,
                debug_manage_hud,
                debug_update_hud,
            ))
            // Sync UI text positions after Bevy UI layout computes positions
            .add_systems(PostUpdate, sync_ui_text_positions.after(UiSystems::Layout));

        // Check if render app exists
        if !app.is_plugin_added::<RenderPlugin>() {
            return;
        }

        let Some(render_app) = app.get_sub_app_mut(RenderApp) else {
            return;
        };

        render_app
            .init_resource::<super::msdf::ExtractedMsdfTexts>()
            .add_systems(ExtractSchedule, extract_msdf_texts)
            .add_systems(Render, prepare_msdf_texts.run_if(resource_exists::<super::msdf::MsdfTextResources>));

        // Add render node to the graph - after Upscaling (final post-processing step)
        use bevy::core_pipeline::core_2d::graph::{Core2d, Node2d};
        render_app
            .add_render_graph_node::<ViewNodeRunner<MsdfTextRenderNode>>(
                Core2d,
                MsdfTextRenderNode::NAME,
            )
            .add_render_graph_edges(Core2d, (Node2d::Upscaling, MsdfTextRenderNode::NAME));
    }

    fn finish(&self, app: &mut App) {
        // Initialize the MSDF atlas in main world
        let mut images = app.world_mut().resource_mut::<Assets<Image>>();
        let atlas = MsdfAtlas::new(&mut images, 1024, 1024);
        app.insert_resource(atlas);

        // Initialize MSDF generator
        let generator = MsdfGenerator::new();
        app.insert_resource(generator);

        // Clone shared resources for render world
        let font_system = app.world().resource::<SharedFontSystem>().clone();

        let Some(render_app) = app.get_sub_app_mut(RenderApp) else {
            return;
        };

        // Initialize render world resources
        render_app.init_resource::<MsdfTextPipeline>();
        render_app.insert_resource(font_system);
        render_app.add_systems(Render, init_msdf_resources.run_if(not(resource_exists::<super::msdf::MsdfTextResources>)));
    }
}

/// Update the text resolution and scale factor when the window resizes.
fn update_text_resolution(
    windows: Query<&Window, With<PrimaryWindow>>,
    mut resolution: ResMut<TextResolution>,
    mut text_metrics: ResMut<TextMetrics>,
) {
    if let Ok(window) = windows.single() {
        let new_width = window.physical_width();
        let new_height = window.physical_height();

        if resolution.width != new_width || resolution.height != new_height {
            resolution.width = new_width;
            resolution.height = new_height;
        }

        // Update scale factor for DPI-aware text rendering
        let scale = window.scale_factor();
        if (text_metrics.scale_factor - scale).abs() > 0.01 {
            text_metrics.scale_factor = scale;
            info!("TextMetrics scale_factor updated: {:.2}", scale);
        }
    }
}

/// Update MSDF generator - poll for completed tasks and queue pending glyphs.
fn update_msdf_generator(
    mut generator: ResMut<MsdfGenerator>,
    mut atlas: ResMut<MsdfAtlas>,
    mut images: ResMut<Assets<Image>>,
    font_system: Res<SharedFontSystem>,
) {
    // Poll for completed generation tasks
    generator.poll_completed(&mut atlas);

    // Queue any pending glyphs
    if !atlas.pending.is_empty() {
        if let Ok(font_system) = font_system.0.lock() {
            generator.queue_pending(&atlas, &font_system);
        }
    }

    // Sync atlas to GPU if dirty
    atlas.sync_to_gpu(&mut images);
}

/// Sync UI text positions from Bevy UI layout to UiTextPositionCache.
fn sync_ui_text_positions(
    mut query: Query<
        (&ComputedNode, &UiGlobalTransform, &mut UiTextPositionCache),
        With<MsdfUiText>,
    >,
) {
    for (computed, global_transform, mut cache) in query.iter_mut() {
        // UiGlobalTransform gives us the center position in screen space
        // (origin at top-left, Y increases downward).
        // Convert to top-left corner for rendering.
        let (_, _, translation) = global_transform.to_scale_angle_translation();
        let size = computed.size();

        // Translation is the center of the node, convert to top-left corner
        cache.left = translation.x - size.x / 2.0;
        cache.top = translation.y - size.y / 2.0;
        cache.width = size.x;
        cache.height = size.y;
    }
}

/// Initialize MsdfTextBuffer for MsdfUiText entities that don't have one.
fn init_ui_text_buffers(
    mut commands: Commands,
    font_system: Res<SharedFontSystem>,
    _text_metrics: Res<TextMetrics>,
    query: Query<(Entity, &MsdfUiText), (Without<MsdfTextBuffer>, Without<MsdfText>)>,
) {
    let Ok(mut font_system) = font_system.0.lock() else {
        return;
    };

    for (entity, ui_text) in query.iter() {
        let metrics = ui_text.metrics;
        let mut buffer = MsdfTextBuffer::new(&mut font_system, metrics);

        // Set the text
        let attrs = cosmic_text::Attrs::new().family(ui_text.family);
        buffer.set_text(&mut font_system, &ui_text.text, attrs, cosmic_text::Shaping::Advanced);

        // Shape the text (triggers glyph generation)
        buffer.visual_line_count(&mut font_system, 800.0);

        // Add buffer and marker components
        commands.entity(entity).insert((
            buffer,
            MsdfText,
            MsdfTextAreaConfig::default(),
        ));
    }
}

/// Request glyphs from the atlas for all text buffers.
fn request_atlas_glyphs(
    mut atlas: ResMut<MsdfAtlas>,
    query: Query<&MsdfTextBuffer, With<MsdfText>>,
) {
    for buffer in query.iter() {
        for glyph in buffer.glyphs() {
            atlas.request(glyph.key);
        }
    }
}

/// Sync UI text positions to MsdfTextAreaConfig every frame.
fn sync_ui_text_config_positions(
    mut query: Query<(&UiTextPositionCache, &mut MsdfTextAreaConfig), With<MsdfUiText>>,
) {
    for (position, mut config) in query.iter_mut() {
        config.left = position.left;
        config.top = position.top;
        config.bounds = super::msdf::TextBounds {
            left: position.left as i32,
            top: position.top as i32,
            right: (position.left + position.width) as i32,
            bottom: (position.top + position.height) as i32,
        };
    }
}

/// Update MsdfTextBuffer when MsdfUiText text changes.
fn update_ui_text_buffers(
    font_system: Res<SharedFontSystem>,
    mut query: Query<(&MsdfUiText, &mut MsdfTextBuffer, &UiTextPositionCache, &mut MsdfTextAreaConfig), Changed<MsdfUiText>>,
) {
    let Ok(mut font_system) = font_system.0.lock() else {
        return;
    };

    for (ui_text, mut buffer, position, mut config) in query.iter_mut() {
        // Update buffer text
        let attrs = cosmic_text::Attrs::new().family(ui_text.family);
        buffer.set_text(&mut font_system, &ui_text.text, attrs, cosmic_text::Shaping::Advanced);

        // Re-shape with position width
        let width = position.width.max(100.0);
        buffer.visual_line_count(&mut font_system, width);

        // Update config position
        config.left = position.left;
        config.top = position.top;
        config.bounds = super::msdf::TextBounds {
            left: position.left as i32,
            top: position.top as i32,
            right: (position.left + position.width) as i32,
            bottom: (position.top + position.height) as i32,
        };
    }
}

/// Debug system: dump MSDF atlas to file on F12 keypress.
#[cfg(debug_assertions)]
fn debug_dump_atlas_on_f12(
    input: Res<ButtonInput<KeyCode>>,
    atlas: Res<MsdfAtlas>,
    mut debug_info: ResMut<MsdfDebugInfo>,
    text_query: Query<&MsdfTextBuffer, With<MsdfText>>,
) {
    // Update debug info every frame
    let stats = atlas.debug_stats();
    debug_info.atlas_size = (stats.width, stats.height);
    debug_info.atlas_glyph_count = stats.glyph_count;
    debug_info.text_area_count = text_query.iter().count();
    debug_info.glyph_count = text_query.iter().map(|b| b.glyphs().len()).sum();

    // Dump atlas on F12
    if input.just_pressed(KeyCode::F12) {
        let path = std::path::Path::new("/tmp/msdf_atlas.raw");
        match atlas.dump_to_file(path) {
            Ok((w, h)) => {
                info!("📊 Atlas stats: {:#?}", stats);
                info!("🎯 Debug info: {} glyphs in {} text areas", debug_info.glyph_count, debug_info.text_area_count);

                // Sample some glyphs for inspection
                debug_info.sample_glyphs.clear();
                for buffer in text_query.iter().take(3) {
                    for glyph in buffer.glyphs().iter().take(5) {
                        if let Some(region) = atlas.get(glyph.key) {
                            const MSDF_PX_PER_EM: f32 = 32.0;
                            let msdf_scale = glyph.font_size / MSDF_PX_PER_EM;
                            debug_info.sample_glyphs.push(super::msdf::DebugGlyph {
                                char_code: glyph.key.glyph_id,
                                glyph_x: glyph.x,
                                glyph_y: glyph.y,
                                font_size: glyph.font_size,
                                region_width: region.width,
                                region_height: region.height,
                                anchor_x: region.anchor_x,
                                anchor_y: region.anchor_y,
                                quad_width: region.width as f32 * msdf_scale,
                                quad_height: region.height as f32 * msdf_scale,
                            });
                        }
                    }
                }
                for (i, g) in debug_info.sample_glyphs.iter().enumerate() {
                    trace!(
                        "Sample glyph {}: glyph_id={}, pos=({:.1}, {:.1}), font_size={:.1}, region={}x{}, anchor=({:.3}, {:.3}), quad={:.1}x{:.1}",
                        i, g.char_code, g.glyph_x, g.glyph_y, g.font_size,
                        g.region_width, g.region_height, g.anchor_x, g.anchor_y,
                        g.quad_width, g.quad_height
                    );
                }

                info!(
                    "💡 To view: convert -size {}x{} -depth 8 rgba:/tmp/msdf_atlas.raw /tmp/msdf_atlas.png && xdg-open /tmp/msdf_atlas.png",
                    w, h
                );
            }
            Err(e) => error!("Failed to dump atlas: {}", e),
        }
    }
}

/// Debug system: toggle MSDF debug overlay with F11.
#[cfg(debug_assertions)]
fn debug_toggle_overlay_f11(
    input: Res<ButtonInput<KeyCode>>,
    mut debug_overlay: ResMut<MsdfDebugOverlay>,
) {
    if input.just_pressed(KeyCode::F11) {
        debug_overlay.mode = debug_overlay.mode.next();
        info!("🔍 MSDF debug overlay: {}", debug_overlay.mode.description());
    }
}

/// Debug system: spawn/despawn debug HUD based on overlay mode.
#[cfg(debug_assertions)]
fn debug_manage_hud(
    mut commands: Commands,
    debug_overlay: Res<MsdfDebugOverlay>,
    existing_hud: Query<Entity, With<MsdfDebugHud>>,
) {
    let should_show = debug_overlay.mode != DebugOverlayMode::Off;
    let hud_exists = !existing_hud.is_empty();

    if should_show && !hud_exists {
        // Spawn debug HUD in top-right corner
        commands.spawn((
            MsdfDebugHud,
            MsdfUiText::new("Debug HUD loading...")
                .with_font_size(11.0)
                .with_color(Color::srgba(0.0, 1.0, 0.5, 0.9)),
            UiTextPositionCache::default(),
            Node {
                position_type: PositionType::Absolute,
                right: Val::Px(10.0),
                top: Val::Px(10.0),
                min_width: Val::Px(300.0),
                min_height: Val::Px(150.0),
                padding: UiRect::all(Val::Px(8.0)),
                ..default()
            },
            BackgroundColor(Color::srgba(0.0, 0.0, 0.0, 0.7)),
            ZIndex(100), // On top of other UI
        ));
        info!("📊 Debug HUD spawned");
    } else if !should_show && hud_exists {
        // Despawn debug HUD
        for entity in existing_hud.iter() {
            commands.entity(entity).despawn();
        }
        info!("📊 Debug HUD despawned");
    }
}

/// Debug system: update debug HUD with current metrics.
#[cfg(debug_assertions)]
fn debug_update_hud(
    debug_overlay: Res<MsdfDebugOverlay>,
    atlas: Res<MsdfAtlas>,
    text_query: Query<(&MsdfTextBuffer, &MsdfTextAreaConfig), With<MsdfText>>,
    mut hud_query: Query<&mut MsdfUiText, With<MsdfDebugHud>>,
) {
    // Only update if debug mode is on
    if debug_overlay.mode == DebugOverlayMode::Off {
        return;
    }

    // Get first glyph from first text area for metrics
    let mut hud_text = String::new();
    hud_text.push_str("=== MSDF Debug HUD ===\n\n");

    // Find first text area with glyphs
    for (buffer, config) in text_query.iter() {
        let glyphs = buffer.glyphs();
        if glyphs.is_empty() {
            continue;
        }

        let glyph = &glyphs[0];
        let font_size = glyph.font_size;

        // Get region if available
        if let Some(region) = atlas.get(glyph.key) {
            const MSDF_PX_PER_EM: f32 = 32.0;
            let msdf_scale = font_size / MSDF_PX_PER_EM;

            let anchor_x_px = region.anchor_x * font_size;
            let anchor_y_px = region.anchor_y * font_size;

            let quad_width = region.width as f32 * msdf_scale;
            let quad_height = region.height as f32 * msdf_scale;

            let final_px_x = config.left + (glyph.x - anchor_x_px) * config.scale;
            let final_px_y = config.top + (glyph.y - anchor_y_px) * config.scale;

            hud_text.push_str(&format!(
                "Glyph: id={}\n\
                 Pen pos: ({:.1}, {:.1})\n\
                 Font size: {:.1}px\n\
                 MSDF scale: {:.3}\n\
                 Text scale: {:.2}\n\
                 \n\
                 Region: {}x{} px\n\
                 Anchor (em): ({:.4}, {:.4})\n\
                 Anchor (px): ({:.1}, {:.1})\n\
                 \n\
                 Quad: {:.1}x{:.1} px\n\
                 Final pos: ({:.1}, {:.1})\n\
                 Text area: ({:.1}, {:.1})\n",
                glyph.key.glyph_id,
                glyph.x, glyph.y,
                font_size,
                msdf_scale,
                config.scale,
                region.width, region.height,
                region.anchor_x, region.anchor_y,
                anchor_x_px, anchor_y_px,
                quad_width, quad_height,
                final_px_x, final_px_y,
                config.left, config.top,
            ));
        } else {
            hud_text.push_str(&format!(
                "Glyph: id={}\n\
                 Pen pos: ({:.1}, {:.1})\n\
                 Font size: {:.1}px\n\
                 (Region not in atlas yet)\n",
                glyph.key.glyph_id,
                glyph.x, glyph.y,
                font_size,
            ));
        }

        // Only show first glyph
        break;
    }

    if hud_text.len() == "=== MSDF Debug HUD ===\n\n".len() {
        hud_text.push_str("No text areas with glyphs found.");
    }

    // Add legend
    hud_text.push_str("\n--- Legend ---\n");
    hud_text.push_str("Green: pen position\n");
    hud_text.push_str("Blue: anchor (offset)\n");
    hud_text.push_str("Yellow: quad corner\n");
    hud_text.push_str("Red: quad outline\n");

    // Update HUD text
    for mut hud in hud_query.iter_mut() {
        hud.text = hud_text.clone();
    }
}
