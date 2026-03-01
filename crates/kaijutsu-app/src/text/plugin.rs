//! Text rendering plugin for Bevy using Vello.
//!
//! Replaces the MSDF pipeline with bevy_vello for vector text rendering.

use bevy::prelude::*;
use bevy::window::PrimaryWindow;
use bevy_vello::VelloPlugin;
use bevy_vello::prelude::*;

use super::components::{KjTextEffects, KjUiText, rainbow_brush};
use super::resources::{FontHandles, TextMetrics};

/// Plugin that enables Vello text rendering in Bevy.
///
/// Replaces `TextRenderPlugin` (MSDF). Sets up:
/// - VelloPlugin (renderer)
/// - Font loading
/// - DPI-aware text metrics
/// - KjUiText → UiVelloText sync system
pub struct KjTextPlugin;

impl Plugin for KjTextPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(VelloPlugin::default())
            .init_resource::<FontHandles>()
            .init_resource::<TextMetrics>()
            .add_systems(Startup, load_fonts)
            .add_systems(Update, (
                sync_text_max_advance,
                sync_text_metrics_from_window,
                update_text_metrics_from_font,
                sync_kj_ui_text,
                animate_rainbow_text,
                super::rich::render_rich_content,
            ));
    }
}

/// Load bundled fonts into VelloFont asset handles.
fn load_fonts(
    asset_server: Res<AssetServer>,
    mut font_handles: ResMut<FontHandles>,
) {
    font_handles.mono = asset_server.load("fonts/NotoMono-Regular.ttf");
    font_handles.serif = asset_server.load("fonts/NotoSerif-Regular.ttf");
    info!("Loaded Vello fonts: NotoMono, NotoSerif");
}

/// Measure actual line height from the loaded font.
///
/// Fires once after the mono font asset loads, replacing the default 24.0
/// with the real Parley-measured line height. This ensures cursor positioning
/// matches what bevy_vello renders.
fn update_text_metrics_from_font(
    font_handles: Res<FontHandles>,
    fonts: Res<Assets<VelloFont>>,
    mut text_metrics: ResMut<TextMetrics>,
) {
    if text_metrics.cell_line_height_from_font {
        return;
    }
    let Some(font) = fonts.get(&font_handles.mono) else {
        return;
    };
    let style = VelloTextStyle {
        font_size: text_metrics.cell_font_size,
        font: font_handles.mono.clone(),
        ..default()
    };
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

/// Sync `KjUiText` changes to the paired `UiVelloText` component.
///
/// When widget systems update `KjUiText.text` or `.color`, this system
/// propagates the change to the Vello rendering component.
fn sync_kj_ui_text(
    font_handles: Res<FontHandles>,
    mut query: Query<(&KjUiText, &mut UiVelloText), Changed<KjUiText>>,
) {
    for (kj_text, mut vello_text) in query.iter_mut() {
        vello_text.value.clone_from(&kj_text.text);
        vello_text.style.font = font_handles.mono.clone();
        vello_text.style.brush = super::components::bevy_color_to_brush(kj_text.color);
        vello_text.style.font_size = kj_text.font_size;
    }
}

/// Sync `max_advance` from the node's content box width.
///
/// Constrains parley text layout to the node's available width, enabling
/// word wrapping for long lines. Without this, text would overflow the node.
///
/// For block cells: `UiTransform` (x: -50%) shifts center→left edge +
/// `VelloTextAnchor::Left` (dy: -text_height/2) vertically centers text.
/// For widgets/overlay: `VelloTextAnchor::Center` (default) relies on
/// `max_advance` ≈ node width for approximate centering.
fn sync_text_max_advance(
    mut query: Query<(&mut UiVelloText, &ComputedNode)>,
) {
    for (mut text, node) in query.iter_mut() {
        let content_width = node.content_box().width();
        if content_width > 0.0 {
            let target = Some(content_width);
            if text.max_advance != target {
                text.max_advance = target;
            }
        }
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
