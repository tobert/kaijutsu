use bevy::{ecs::hierarchy::ChildSpawnerCommands, prelude::*};

use super::theme::Theme;

/// Message fired when a message should be added to the context area
#[derive(Message)]
pub struct MessageEvent {
    pub sender: String,
    pub content: String,
}

/// Marker for the context area container
#[derive(Component)]
pub struct ContextArea;

/// Marker for individual messages
#[derive(Component)]
pub struct MessageBlock;

/// Spawn a message into the context area
pub fn spawn_messages(
    mut commands: Commands,
    mut events: MessageReader<MessageEvent>,
    context_query: Query<Entity, With<ContextArea>>,
    theme: Res<Theme>,
) {
    let Ok(context_entity) = context_query.single() else {
        return;
    };

    for event in events.read() {
        let sender = event.sender.clone();
        let content = event.content.clone();
        let accent2 = theme.accent2;
        let fg = theme.fg;

        commands.entity(context_entity).with_children(|parent| {
            spawn_message(parent, &sender, &content, accent2, fg);
        });
    }
}

fn spawn_message(
    parent: &mut ChildSpawnerCommands,
    sender: &str,
    content: &str,
    sender_color: Color,
    content_color: Color,
) {
    parent
        .spawn((
            Node {
                flex_direction: FlexDirection::Row,
                column_gap: Val::Px(8.0),
                ..default()
            },
            MessageBlock,
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
