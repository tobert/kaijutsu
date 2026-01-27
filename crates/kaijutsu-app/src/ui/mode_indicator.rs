//! Mode indicator UI - shows current vim-style editor mode.
//!
//! The mode indicator is spawned in main.rs as part of the status bar
//! (flex child, not absolute positioned). This module provides the
//! marker component and update system.

use bevy::prelude::*;

use crate::cell::{CurrentMode, EditorMode, InputKind};
use crate::text::{bevy_to_glyphon_color, GlyphonUiText};
use crate::ui::theme::Theme;

/// Marker component for the mode indicator text.
#[derive(Component)]
pub struct ModeIndicator;

/// Update mode indicator when mode changes.
pub fn update_mode_indicator(
    mode: Res<CurrentMode>,
    theme: Res<Theme>,
    mut indicators: Query<&mut GlyphonUiText, With<ModeIndicator>>,
) {
    if !mode.is_changed() {
        return;
    }

    // Color based on mode (from theme)
    let color = match mode.0 {
        EditorMode::Normal => theme.mode_normal,
        EditorMode::Input(InputKind::Chat) => theme.mode_chat,
        EditorMode::Input(InputKind::Shell) => theme.mode_shell,
        EditorMode::Visual => theme.mode_visual,
    };

    for mut ui_text in indicators.iter_mut() {
        ui_text.text = mode.0.name().to_string();
        ui_text.color = bevy_to_glyphon_color(color);
    }
}
