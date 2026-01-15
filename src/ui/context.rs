use bevy::{ecs::hierarchy::ChildSpawnerCommands, prelude::*};

use super::theme::Theme;
use crate::state::{mode::Mode, nav::NavigationState};

/// Message fired when a message should be added to the context area
#[derive(Message)]
pub struct MessageEvent {
    pub sender: String,
    pub content: String,
}

/// Marker for the context area container
#[derive(Component)]
pub struct ContextArea;

/// Marker for individual messages with their index
#[derive(Component)]
pub struct MessageBlock(pub usize);

/// Tracks total message count for navigation bounds
#[derive(Resource, Default)]
pub struct MessageCount(pub usize);

/// Spawn a message into the context area
pub fn spawn_messages(
    mut commands: Commands,
    mut events: MessageReader<MessageEvent>,
    context_query: Query<Entity, With<ContextArea>>,
    theme: Res<Theme>,
    mut count: ResMut<MessageCount>,
) {
    let Ok(context_entity) = context_query.single() else {
        return;
    };

    for event in events.read() {
        let sender = event.sender.clone();
        let content = event.content.clone();
        let accent2 = theme.accent2;
        let fg = theme.fg;
        let index = count.0;
        count.0 += 1;

        commands.entity(context_entity).with_children(|parent| {
            spawn_message(parent, &sender, &content, accent2, fg, index);
        });
    }
}

fn spawn_message(
    parent: &mut ChildSpawnerCommands,
    sender: &str,
    content: &str,
    sender_color: Color,
    content_color: Color,
    index: usize,
) {
    parent
        .spawn((
            Node {
                flex_direction: FlexDirection::Row,
                column_gap: Val::Px(8.0),
                padding: UiRect::all(Val::Px(4.0)),
                ..default()
            },
            MessageBlock(index),
        ))
        .with_children(|msg| {
            msg.spawn((
                Text::new(format!("{}:", sender)),
                TextFont {
                    font_size: 14.0,
                    ..default()
                },
                TextColor(sender_color),
            ));
            msg.spawn((
                Text::new(content),
                TextFont {
                    font_size: 14.0,
                    ..default()
                },
                TextColor(content_color),
            ));
        });
}

/// Handle j/k navigation in Normal mode
pub fn handle_navigation(
    keys: Res<ButtonInput<KeyCode>>,
    mode: Res<State<Mode>>,
    mut nav: ResMut<NavigationState>,
    count: Res<MessageCount>,
) {
    if *mode.get() != Mode::Normal {
        return;
    }

    if keys.just_pressed(KeyCode::KeyJ) {
        nav.select_next(count.0);
    }
    if keys.just_pressed(KeyCode::KeyK) {
        nav.select_prev();
    }
}

/// Update visual highlight based on selection
pub fn update_selection_highlight(
    nav: Res<NavigationState>,
    theme: Res<Theme>,
    mut query: Query<(&MessageBlock, &mut BackgroundColor)>,
) {
    if !nav.is_changed() {
        return;
    }

    for (block, mut bg) in &mut query {
        *bg = if Some(block.0) == nav.selected {
            BackgroundColor(theme.border) // Subtle highlight
        } else {
            BackgroundColor(Color::NONE)
        };
    }
}
