//! Text rendering plugin for Bevy using Vello.
//!
//! Replaces the MSDF pipeline with bevy_vello for vector text rendering.

use bevy::prelude::*;
use bevy::window::PrimaryWindow;
use bevy_vello::VelloPlugin;
use bevy_vello::prelude::*;

use super::components::KjUiText;
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
                sync_text_metrics_from_window,
                sync_kj_ui_text,
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
