use bevy::{ecs::hierarchy::ChildSpawnerCommands, prelude::*};

use super::theme::Theme;
use crate::state::{mode::Mode, nav::NavigationState};

/// Types of rows in the context DAG
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RowType {
    #[default]
    User,
    Agent,
    ToolCall,
    ToolResult,
    System,
}

impl RowType {
    /// Icon prefix for this row type
    pub fn icon(&self) -> &'static str {
        match self {
            RowType::User => "",
            RowType::Agent => "◉",
            RowType::ToolCall => "⚙",
            RowType::ToolResult => "└─",
            RowType::System => "⚡",
        }
    }

    /// Whether this row type can have children
    pub fn can_have_children(&self) -> bool {
        matches!(self, RowType::Agent | RowType::ToolCall)
    }
}

/// Message fired when a block should be added to the context area
#[derive(Message)]
pub struct MessageEvent {
    pub sender: String,
    pub content: String,
    pub row_type: RowType,
    pub parent_id: Option<usize>,
}

impl MessageEvent {
    /// Simple user message
    pub fn user(sender: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            sender: sender.into(),
            content: content.into(),
            row_type: RowType::User,
            parent_id: None,
        }
    }

    /// Agent response
    pub fn agent(sender: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            sender: sender.into(),
            content: content.into(),
            row_type: RowType::Agent,
            parent_id: None,
        }
    }

    /// System message
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            sender: "system".into(),
            content: content.into(),
            row_type: RowType::System,
            parent_id: None,
        }
    }

    /// Tool call (child of an agent message)
    pub fn tool_call(tool_name: impl Into<String>, parent_id: usize) -> Self {
        Self {
            sender: tool_name.into(),
            content: String::new(),
            row_type: RowType::ToolCall,
            parent_id: Some(parent_id),
        }
    }

    /// Tool result (child of a tool call)
    pub fn tool_result(content: impl Into<String>, parent_id: usize) -> Self {
        Self {
            sender: String::new(),
            content: content.into(),
            row_type: RowType::ToolResult,
            parent_id: Some(parent_id),
        }
    }
}

/// Marker for the context area container
#[derive(Component)]
pub struct ContextArea;

/// A block in the context DAG
#[derive(Component)]
pub struct ContextBlock {
    pub id: usize,
    pub row_type: RowType,
    pub parent_id: Option<usize>,
    pub collapsed: bool,
    pub depth: usize,
}

/// Marker for the collapse indicator text
#[derive(Component)]
pub struct CollapseIndicator(pub usize);

/// Marker for the content area of a block (hidden when collapsed)
#[derive(Component)]
pub struct BlockContent(pub usize);

/// Tracks total message count for navigation bounds
#[derive(Resource, Default)]
pub struct MessageCount(pub usize);

/// Spawn a block into the context area
pub fn spawn_messages(
    mut commands: Commands,
    mut events: MessageReader<MessageEvent>,
    context_query: Query<Entity, With<ContextArea>>,
    theme: Res<Theme>,
    mut count: ResMut<MessageCount>,
    block_query: Query<&ContextBlock>,
) {
    let Ok(context_entity) = context_query.single() else {
        return;
    };

    for event in events.read() {
        let id = count.0;
        count.0 += 1;

        // Calculate depth from parent chain
        let depth = if let Some(parent_id) = event.parent_id {
            block_query
                .iter()
                .find(|b| b.id == parent_id)
                .map(|b| b.depth + 1)
                .unwrap_or(0)
        } else {
            0
        };

        let row_type = event.row_type;
        let sender = event.sender.clone();
        let content = event.content.clone();

        commands.entity(context_entity).with_children(|parent| {
            spawn_block(parent, &theme, id, row_type, depth, &sender, &content);
        });
    }
}

fn spawn_block(
    parent: &mut ChildSpawnerCommands,
    theme: &Theme,
    id: usize,
    row_type: RowType,
    depth: usize,
    sender: &str,
    content: &str,
) {
    let border_color = match row_type {
        RowType::User => theme.row_user,
        RowType::Agent => theme.row_agent,
        RowType::ToolCall => theme.row_tool,
        RowType::ToolResult => theme.row_result,
        RowType::System => theme.row_system,
    };

    let indent = depth as f32 * 16.0;
    let can_collapse = row_type.can_have_children();

    parent
        .spawn((
            Node {
                flex_direction: FlexDirection::Column,
                margin: UiRect::left(Val::Px(indent)),
                padding: UiRect::all(Val::Px(8.0)),
                border: UiRect::left(Val::Px(3.0)),
                ..default()
            },
            BorderColor::all(border_color),
            BackgroundColor(Color::NONE),
            ContextBlock {
                id,
                row_type,
                parent_id: None,
                collapsed: false,
                depth,
            },
        ))
        .with_children(|block| {
            // Header row: [collapse indicator] [icon] sender
            block
                .spawn(Node {
                    flex_direction: FlexDirection::Row,
                    column_gap: Val::Px(8.0),
                    align_items: AlignItems::Center,
                    ..default()
                })
                .with_children(|header| {
                    // Collapse indicator (only for collapsible types)
                    if can_collapse {
                        header.spawn((
                            Text::new("▼"),
                            TextFont {
                                font_size: 12.0,
                                ..default()
                            },
                            TextColor(theme.fg_dim),
                            CollapseIndicator(id),
                        ));
                    }

                    // Row type icon
                    let icon = row_type.icon();
                    if !icon.is_empty() {
                        header.spawn((
                            Text::new(icon),
                            TextFont {
                                font_size: 14.0,
                                ..default()
                            },
                            TextColor(border_color),
                        ));
                    }

                    // Sender
                    if !sender.is_empty() {
                        header.spawn((
                            Text::new(format!("{}:", sender)),
                            TextFont {
                                font_size: 14.0,
                                ..default()
                            },
                            TextColor(border_color),
                        ));
                    }
                });

            // Content (can be hidden when collapsed)
            if !content.is_empty() {
                block.spawn((
                    Node {
                        margin: UiRect::top(Val::Px(4.0)),
                        ..default()
                    },
                    BlockContent(id),
                )).with_children(|content_node| {
                    content_node.spawn((
                        Text::new(content),
                        TextFont {
                            font_size: 14.0,
                            ..default()
                        },
                        TextColor(theme.fg),
                    ));
                });
            }
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

/// Toggle collapse on Enter key
pub fn handle_collapse_toggle(
    keys: Res<ButtonInput<KeyCode>>,
    mode: Res<State<Mode>>,
    nav: Res<NavigationState>,
    mut block_query: Query<&mut ContextBlock>,
    mut indicator_query: Query<(&CollapseIndicator, &mut Text)>,
    mut content_query: Query<(&BlockContent, &mut Visibility)>,
) {
    if *mode.get() != Mode::Normal {
        return;
    }

    if !keys.just_pressed(KeyCode::Enter) {
        return;
    }

    let Some(selected_id) = nav.selected else {
        return;
    };

    // Find and toggle the selected block
    for mut block in &mut block_query {
        if block.id == selected_id && block.row_type.can_have_children() {
            block.collapsed = !block.collapsed;

            // Update collapse indicator
            for (indicator, mut text) in &mut indicator_query {
                if indicator.0 == selected_id {
                    **text = if block.collapsed { "▶" } else { "▼" }.to_string();
                }
            }

            // Toggle content visibility
            for (content, mut vis) in &mut content_query {
                if content.0 == selected_id {
                    *vis = if block.collapsed {
                        Visibility::Hidden
                    } else {
                        Visibility::Inherited
                    };
                }
            }
        }
    }
}

/// Update visual highlight based on selection
pub fn update_selection_highlight(
    nav: Res<NavigationState>,
    theme: Res<Theme>,
    mut query: Query<(&ContextBlock, &mut BackgroundColor)>,
) {
    if !nav.is_changed() {
        return;
    }

    for (block, mut bg) in &mut query {
        *bg = if Some(block.id) == nav.selected {
            BackgroundColor(theme.border)
        } else {
            BackgroundColor(Color::NONE)
        };
    }
}
