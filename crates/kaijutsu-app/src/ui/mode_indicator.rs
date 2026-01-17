//! Mode indicator UI - shows current vim-style editor mode.

use bevy::prelude::*;

use crate::cell::{CurrentMode, EditorMode};
use crate::ui::theme::Theme;

/// Marker component for the mode indicator text.
#[derive(Component)]
pub struct ModeIndicator;

/// Spawn the mode indicator UI.
pub fn setup_mode_indicator(mut commands: Commands, theme: Res<Theme>) {
    // Mode indicator in bottom-left corner
    commands.spawn((
        ModeIndicator,
        Text::new("NORMAL"),
        TextFont {
            font_size: 14.0,
            ..default()
        },
        TextColor(theme.fg_dim),
        Node {
            position_type: PositionType::Absolute,
            bottom: Val::Px(20.0),
            left: Val::Px(20.0),
            padding: UiRect::all(Val::Px(8.0)),
            ..default()
        },
        BackgroundColor(theme.panel_bg),
    ));
}

/// Update mode indicator when mode changes.
pub fn update_mode_indicator(
    mode: Res<CurrentMode>,
    theme: Res<Theme>,
    mut indicators: Query<&mut Text, With<ModeIndicator>>,
    mut colors: Query<&mut TextColor, With<ModeIndicator>>,
) {
    if !mode.is_changed() {
        return;
    }

    for mut text in indicators.iter_mut() {
        text.0 = mode.0.name().to_string();
    }

    // Color based on mode
    let color = match mode.0 {
        EditorMode::Normal => theme.fg_dim,
        EditorMode::Insert => Color::srgb(0.4, 0.8, 0.4), // Green
        EditorMode::Command => Color::srgb(0.9, 0.7, 0.2), // Yellow/orange
        EditorMode::Visual => Color::srgb(0.7, 0.4, 0.9), // Purple
    };

    for mut text_color in colors.iter_mut() {
        text_color.0 = color;
    }
}
