//! Text rendering plugin for Bevy using MSDF.
//!
//! Sets up MSDF text rendering integrated with Bevy's render pipeline.

use bevy::prelude::*;
use bevy::render::{
    render_graph::{RenderGraphExt, ViewNodeRunner},
    Extract, Render, RenderApp, RenderPlugin,
};
use bevy::ui::{ComputedNode, UiGlobalTransform, UiSystems};
use bevy::window::PrimaryWindow;

use super::msdf::{
    extract_msdf_texts, init_msdf_resources, prepare_msdf_texts, MsdfAtlas, MsdfGenerator,
    MsdfText, MsdfTextAreaConfig, MsdfTextBuffer, MsdfTextPipeline, MsdfTextRenderNode,
    MsdfUiText, UiTextPositionCache,
};
use super::resources::*;

/// Plugin that enables MSDF text rendering in Bevy.
pub struct TextRenderPlugin;

impl Plugin for TextRenderPlugin {
    fn build(&self, app: &mut App) {
        // Main world resources
        app.init_resource::<SharedFontSystem>()
            .init_resource::<TextResolution>()
            .init_resource::<TextMetrics>()
            .add_systems(Update, (
                update_text_resolution,
                init_ui_text_buffers,
                update_ui_text_buffers,
                sync_ui_text_config_positions,
                request_atlas_glyphs,
                update_msdf_generator,
            ).chain())
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
