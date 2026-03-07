//! Text rendering plugin for Bevy using Vello.
//!
//! Replaces the MSDF pipeline with bevy_vello for vector text rendering.

use bevy::prelude::*;
use bevy::window::PrimaryWindow;
use bevy_vello::VelloPlugin;
use bevy_vello::prelude::*;

use super::components::{KjTextEffects, rainbow_brush};
use super::resources::{FontHandles, TextMetrics};

/// Plugin that enables Vello text rendering in Bevy.
///
/// Replaces `TextRenderPlugin` (MSDF). Sets up:
/// - VelloPlugin (renderer)
/// - Font loading
/// - DPI-aware text metrics
pub struct KjTextPlugin;

impl Plugin for KjTextPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(VelloPlugin::default())
            .init_resource::<FontHandles>()
            .init_resource::<TextMetrics>()
            .add_systems(Startup, load_fonts)
            .add_systems(Update, (
                sync_text_metrics_from_window,
                update_text_metrics_from_font,
                animate_rainbow_text,
                // render_rich_content is registered in CellPlugin (CellPhase::Buffer)
                // so it runs after sync_block_cell_buffers + ApplyDeferred.
                // NOTE: sync_text_max_advance is in view/render.rs (CellPhase::Layout)
                // with Changed<ComputedNode> filter — intentionally not here.
            ));
    }
}

/// Load bundled fonts into VelloFont asset handles.
fn load_fonts(
    asset_server: Res<AssetServer>,
    mut font_handles: ResMut<FontHandles>,
) {
    font_handles.mono = asset_server.load("fonts/CascadiaCodeNF.ttf");
    font_handles.serif = asset_server.load("fonts/NotoSerif-Regular.ttf");
    font_handles.cjk = asset_server.load("fonts/NotoSansCJKJP-Light.ttf");
    info!("Loaded Vello fonts: CascadiaCodeNF, NotoSerif, NotoSansCJKJP");
}

/// Measure actual line height and character width from the loaded font.
///
/// Fires once after the mono font asset loads, replacing the defaults with
/// real Parley-measured metrics. This ensures cursor positioning matches
/// what bevy_vello renders — critical for accurate cursor placement.
fn update_text_metrics_from_font(
    font_handles: Res<FontHandles>,
    fonts: Res<Assets<VelloFont>>,
    mut text_metrics: ResMut<TextMetrics>,
) {
    // Early exit if both metrics are already measured
    if text_metrics.cell_line_height_from_font && text_metrics.cell_char_width_from_font {
        return;
    }
    let Some(font) = fonts.get(&font_handles.mono) else {
        return;
    };
    let style = VelloTextStyle {
        font_size: text_metrics.cell_font_size,
        font: font_handles.mono.clone(),
        font_axes: VelloFontAxes {
            weight: Some(200.0),
            ..default()
        },
        ..default()
    };

    // Measure line height from "X"
    if !text_metrics.cell_line_height_from_font {
        let layout = font.layout("X", &style, VelloTextAlign::Left, None);
        if let Some(line) = layout.lines().next() {
            let measured = line.metrics().line_height;
            if measured > 0.0 {
                info!(
                    "TextMetrics: cell_line_height updated from font: {:.1} → {:.1}",
                    text_metrics.cell_line_height, measured
                );
                text_metrics.cell_line_height = measured;
                text_metrics.cell_line_height_from_font = true;
            }
        }
    }

    // Measure character width from "M" (standard em-width reference)
    if !text_metrics.cell_char_width_from_font {
        let layout = font.layout("M", &style, VelloTextAlign::Left, None);
        if let Some(line) = layout.lines().next() {
            if let Some(run) = line.runs().next() {
                let advance = run.advance();
                if advance > 0.0 {
                    info!(
                        "TextMetrics: cell_char_width updated from font: {:.1} → {:.1}",
                        text_metrics.cell_char_width, advance
                    );
                    text_metrics.cell_char_width = advance;
                    text_metrics.cell_char_width_from_font = true;
                }
            }
        }
    }
}

/// Sync DPI scale factor from the primary window.
fn sync_text_metrics_from_window(
    windows: Query<&Window, With<PrimaryWindow>>,
    mut text_metrics: ResMut<TextMetrics>,
) {
    let Ok(window) = windows.single() else {
        return;
    };

    let scale = window.scale_factor();
    if (text_metrics.scale_factor - scale).abs() > 0.01 {
        text_metrics.scale_factor = scale;
        info!("TextMetrics scale_factor updated: {:.2}", scale);
    }
}

/// Animate rainbow gradient text each frame.
///
/// Entities with `KjTextEffects { rainbow: true }` get a scrolling
/// linear gradient brush. The phase advances with elapsed time,
/// creating a smooth cycling rainbow effect.
fn animate_rainbow_text(
    time: Res<Time>,
    mut query: Query<(&KjTextEffects, &mut UiVelloText)>,
) {
    // Phase cycles 0→1 over ~4 seconds
    let phase = (time.elapsed_secs() * 0.25) % 1.0;

    for (effects, mut vello_text) in query.iter_mut() {
        if !effects.rainbow {
            continue;
        }
        // Alpha from current brush (preserve timeline dimming)
        let alpha = match &vello_text.style.brush {
            vello::peniko::Brush::Solid(c) => c.components[3],
            _ => 1.0,
        };
        vello_text.style.brush = rainbow_brush(phase, alpha);
    }
}
