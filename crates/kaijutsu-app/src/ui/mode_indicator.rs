//! Mode indicator UI - shows current vim-style editor mode.

use bevy::prelude::*;

use crate::cell::{CurrentMode, EditorMode};
use crate::text::{bevy_to_glyphon_color, GlyphonUiText, UiTextPositionCache};
use crate::ui::theme::Theme;

/// Marker component for the mode indicator text.
#[derive(Component)]
pub struct ModeIndicator;

/// Spawn the mode indicator UI.
pub fn setup_mode_indicator(mut commands: Commands, theme: Res<Theme>) {
    // Mode indicator in bottom-left corner (uses glyphon for CJK support)
    commands.spawn((
        ModeIndicator,
        GlyphonUiText::new("NORMAL")
            .with_font_size(14.0)
            .with_color(theme.fg_dim),
        UiTextPositionCache::default(),
        Node {
            position_type: PositionType::Absolute,
            bottom: Val::Px(20.0),
            left: Val::Px(20.0),
            padding: UiRect::all(Val::Px(8.0)),
            min_width: Val::Px(80.0),
            min_height: Val::Px(20.0),
            ..default()
        },
        BackgroundColor(theme.panel_bg),
    ));
}

/// Update mode indicator when mode changes.
pub fn update_mode_indicator(
    mode: Res<CurrentMode>,
    theme: Res<Theme>,
    mut indicators: Query<&mut GlyphonUiText, With<ModeIndicator>>,
) {
    if !mode.is_changed() {
        return;
    }

    // Color based on mode
    let color = match mode.0 {
        EditorMode::Normal => theme.fg_dim,
        EditorMode::Insert => Color::srgb(0.4, 0.8, 0.4),  // Green
        EditorMode::Command => Color::srgb(0.9, 0.7, 0.2), // Yellow/orange
        EditorMode::Visual => Color::srgb(0.7, 0.4, 0.9),  // Purple
    };

    for mut ui_text in indicators.iter_mut() {
        ui_text.text = mode.0.name().to_string();
        ui_text.color = bevy_to_glyphon_color(color);
    }
}
