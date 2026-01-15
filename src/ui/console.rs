use bevy::prelude::*;

use super::theme::Theme;

/// Resource tracking console visibility
#[derive(Resource, Default)]
pub struct ConsoleState {
    pub visible: bool,
}

/// Marker for the console panel
#[derive(Component)]
pub struct ConsolePanel;

/// Spawn the console panel (hidden by default)
pub fn setup_console(mut commands: Commands, theme: Res<Theme>) {
    commands.spawn((
        Node {
            position_type: PositionType::Absolute,
            top: Val::Px(40.0), // Below title bar
            left: Val::Px(180.0), // After sidebar
            right: Val::Px(0.0),
            height: Val::Percent(40.0),
            flex_direction: FlexDirection::Column,
            padding: UiRect::all(Val::Px(16.0)),
            border: UiRect::bottom(Val::Px(2.0)),
            ..default()
        },
        BackgroundColor(Color::srgba(0.02, 0.02, 0.05, 0.95)),
        BorderColor::all(theme.accent),
        Visibility::Hidden,
        ConsolePanel,
    ))
    .with_children(|console| {
        // Header
        console.spawn((
            Text::new("【kaish】 Console"),
            TextFont {
                font_size: 16.0,
                ..default()
            },
            TextColor(theme.accent),
        ));

        // Placeholder content
        console.spawn((
            Node {
                margin: UiRect::top(Val::Px(12.0)),
                ..default()
            },
        ))
        .with_children(|content| {
            content.spawn((
                Text::new("Console coming soon..."),
                TextFont {
                    font_size: 14.0,
                    ..default()
                },
                TextColor(theme.fg_dim),
            ));
            content.spawn((
                Text::new("\n\nThis will be a shared kaish kernel session."),
                TextFont {
                    font_size: 12.0,
                    ..default()
                },
                TextColor(theme.fg_dim),
            ));
            content.spawn((
                Text::new("\nPress ` to close."),
                TextFont {
                    font_size: 12.0,
                    ..default()
                },
                TextColor(theme.fg_dim),
            ));
        });
    });
}

/// Toggle console visibility with backtick
pub fn toggle_console(
    keys: Res<ButtonInput<KeyCode>>,
    mut state: ResMut<ConsoleState>,
    mut query: Query<&mut Visibility, With<ConsolePanel>>,
) {
    if keys.just_pressed(KeyCode::Backquote) {
        state.visible = !state.visible;

        for mut vis in &mut query {
            *vis = if state.visible {
                Visibility::Visible
            } else {
                Visibility::Hidden
            };
        }
    }
}
