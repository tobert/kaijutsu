//! Create context dialog for the constellation
//!
//! Provides a modal dialog that appears when clicking the "+" node,
//! allowing users to enter a name for a new context.

use bevy::input::keyboard::{Key, KeyboardInput};
use bevy::prelude::*;
use uuid::Uuid;

use crate::connection::{ConnectionCommand, ConnectionCommands};
use crate::text::{bevy_to_rgba8, MsdfUiText, UiTextPositionCache};
use crate::ui::theme::Theme;

// ============================================================================
// RESOURCES
// ============================================================================

/// Tracks whether a modal dialog is open.
///
/// When true, other keyboard input handlers (e.g., prompt input) should
/// skip processing to prevent input leakage to underlying UI elements.
#[derive(Resource, Default)]
pub struct ModalDialogOpen(pub bool);

// ============================================================================
// COMPONENTS
// ============================================================================

/// Marker for the "+" create context node in the constellation
#[derive(Component, Default, Reflect)]
#[reflect(Component)]
pub struct CreateContextNode;

/// Marker for the create context dialog (modal overlay)
#[derive(Component)]
pub struct CreateContextDialog;

/// Marker for the text input field in the dialog
#[derive(Component)]
pub struct ContextNameInput {
    /// Current input text
    pub text: String,
}

impl Default for ContextNameInput {
    fn default() -> Self {
        Self {
            text: String::new(),
        }
    }
}

/// Marker for the submit button
#[derive(Component)]
pub struct CreateContextSubmit;

/// Marker for the cancel button
#[derive(Component)]
pub struct CreateContextCancel;

/// Marker for the text display in the input field (so we can update it)
#[derive(Component)]
pub struct InputTextDisplay;

// ============================================================================
// SYSTEMS
// ============================================================================

/// Setup the create dialog systems
pub fn setup_create_dialog_systems(app: &mut App) {
    app.init_resource::<ModalDialogOpen>()
        .register_type::<CreateContextNode>()
        .add_systems(
            Update,
            (
                handle_create_node_click,
                handle_dialog_input,
                handle_dialog_buttons,
                handle_dialog_escape,
            ),
        );
}

/// Spawn the "+" create context node (called from render.rs)
pub fn spawn_create_context_node(
    commands: &mut Commands,
    container_entity: Entity,
    theme: &Theme,
    pulse_materials: &mut Assets<crate::shaders::PulseRingMaterial>,
) {
    use crate::shaders::PulseRingMaterial;
    use crate::ui::theme::color_to_vec4;

    // Position the "+" node at the right edge of the constellation, not in orbital rotation
    let container_center =
        theme.constellation_layout_radius + theme.constellation_node_size_focused / 2.0;
    let node_size = theme.constellation_node_size * 0.8; // Slightly smaller
    let half_size = node_size / 2.0;

    // Position at 3 o'clock (right side), slightly outside the main orbit
    let x_pos = theme.constellation_layout_radius * 1.3;
    let y_pos = 0.0;

    // Create a distinct material for the + node (dimmer, using fg_dim color)
    let material = pulse_materials.add(PulseRingMaterial {
        color: color_to_vec4(theme.fg_dim.with_alpha(0.5)),
        params: Vec4::new(2.0, 0.04, 0.2, 1.0), // Fewer rings, slower pulse
        time: Vec4::ZERO,
    });

    let node_entity = commands
        .spawn((
            CreateContextNode,
            Node {
                position_type: PositionType::Absolute,
                left: Val::Px(container_center + x_pos - half_size),
                top: Val::Px(container_center + y_pos - half_size),
                width: Val::Px(node_size),
                height: Val::Px(node_size),
                ..default()
            },
            MaterialNode(material),
            Interaction::None,
        ))
        .with_children(|parent| {
            // "+" symbol in the center
            parent.spawn((
                Node {
                    position_type: PositionType::Absolute,
                    width: Val::Percent(100.0),
                    height: Val::Percent(100.0),
                    justify_content: JustifyContent::Center,
                    align_items: AlignItems::Center,
                    ..default()
                },
            ))
            .with_children(|center| {
                center.spawn((
                    MsdfUiText::new("+")
                        .with_font_size(32.0)
                        .with_color(theme.fg_dim),
                    UiTextPositionCache::default(),
                    Node::default(),
                ));
            });

            // Label below
            parent
                .spawn((
                    Node {
                        position_type: PositionType::Absolute,
                        bottom: Val::Px(-20.0),
                        left: Val::Percent(50.0),
                        margin: UiRect::left(Val::Px(-40.0)),
                        width: Val::Px(80.0),
                        justify_content: JustifyContent::Center,
                        ..default()
                    },
                    BackgroundColor(theme.panel_bg.with_alpha(0.7)),
                ))
                .with_children(|label_bg| {
                    label_bg.spawn((
                        MsdfUiText::new("new")
                            .with_font_size(10.0)
                            .with_color(theme.fg_dim),
                        UiTextPositionCache::default(),
                        Node::default(),
                    ));
                });
        })
        .id();

    commands.entity(container_entity).add_child(node_entity);
    info!("Spawned create context (+) node");
}

/// Handle clicks on the create context node
fn handle_create_node_click(
    mut commands: Commands,
    theme: Res<Theme>,
    mut modal_state: ResMut<ModalDialogOpen>,
    create_nodes: Query<&Interaction, (Changed<Interaction>, With<CreateContextNode>)>,
    existing_dialog: Query<Entity, With<CreateContextDialog>>,
) {
    // Check if dialog already exists
    if !existing_dialog.is_empty() {
        return;
    }

    for interaction in create_nodes.iter() {
        if *interaction == Interaction::Pressed {
            info!("Create context node clicked - spawning dialog");
            modal_state.0 = true;
            spawn_create_dialog(&mut commands, &theme);
        }
    }
}

/// Spawn the create context dialog (modal)
fn spawn_create_dialog(commands: &mut Commands, theme: &Theme) {
    // Modal overlay (darkens background)
    commands
        .spawn((
            CreateContextDialog,
            Node {
                position_type: PositionType::Absolute,
                left: Val::Px(0.0),
                top: Val::Px(0.0),
                width: Val::Percent(100.0),
                height: Val::Percent(100.0),
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                ..default()
            },
            BackgroundColor(Color::srgba(0.0, 0.0, 0.0, 0.6)),
            ZIndex(crate::constants::ZLayer::MODAL),
            // Capture interactions to prevent click-through
            Interaction::None,
        ))
        .with_children(|overlay| {
            // Dialog box
            overlay
                .spawn((
                    Node {
                        width: Val::Px(320.0),
                        height: Val::Px(160.0),
                        flex_direction: FlexDirection::Column,
                        padding: UiRect::all(Val::Px(20.0)),
                        border_radius: BorderRadius::all(Val::Px(8.0)),
                        row_gap: Val::Px(16.0),
                        ..default()
                    },
                    BackgroundColor(theme.panel_bg),
                    BorderColor::all(theme.border),
                    Outline::new(Val::Px(1.0), Val::ZERO, theme.border),
                ))
                .with_children(|dialog| {
                    // Title
                    dialog.spawn((
                        MsdfUiText::new("Create New Context")
                            .with_font_size(16.0)
                            .with_color(theme.fg),
                        UiTextPositionCache::default(),
                        Node::default(),
                    ));

                    // Input field container
                    dialog
                        .spawn((
                            ContextNameInput::default(),
                            Node {
                                width: Val::Percent(100.0),
                                height: Val::Px(36.0),
                                padding: UiRect::all(Val::Px(8.0)),
                                border_radius: BorderRadius::all(Val::Px(4.0)),
                                ..default()
                            },
                            BackgroundColor(theme.bg),
                            BorderColor::all(theme.border),
                            Outline::new(Val::Px(1.0), Val::ZERO, theme.accent),
                        ))
                        .with_children(|input_field| {
                            // Input text display (starts with placeholder)
                            input_field.spawn((
                                InputTextDisplay,
                                MsdfUiText::new("Enter context name...")
                                    .with_font_size(14.0)
                                    .with_color(theme.fg_dim),
                                UiTextPositionCache::default(),
                                Node::default(),
                            ));
                        });

                    // Button row
                    dialog
                        .spawn(Node {
                            width: Val::Percent(100.0),
                            flex_direction: FlexDirection::Row,
                            justify_content: JustifyContent::End,
                            column_gap: Val::Px(8.0),
                            ..default()
                        })
                        .with_children(|buttons| {
                            // Cancel button
                            buttons
                                .spawn((
                                    CreateContextCancel,
                                    Node {
                                        padding: UiRect::axes(Val::Px(16.0), Val::Px(8.0)),
                                        border_radius: BorderRadius::all(Val::Px(4.0)),
                                        ..default()
                                    },
                                    BackgroundColor(theme.fg_dim.with_alpha(0.2)),
                                    Interaction::None,
                                ))
                                .with_children(|btn| {
                                    btn.spawn((
                                        MsdfUiText::new("Cancel")
                                            .with_font_size(12.0)
                                            .with_color(theme.fg_dim),
                                        UiTextPositionCache::default(),
                                        Node::default(),
                                    ));
                                });

                            // Create button
                            buttons
                                .spawn((
                                    CreateContextSubmit,
                                    Node {
                                        padding: UiRect::axes(Val::Px(16.0), Val::Px(8.0)),
                                        border_radius: BorderRadius::all(Val::Px(4.0)),
                                        ..default()
                                    },
                                    BackgroundColor(theme.accent.with_alpha(0.3)),
                                    Interaction::None,
                                ))
                                .with_children(|btn| {
                                    btn.spawn((
                                        MsdfUiText::new("Create")
                                            .with_font_size(12.0)
                                            .with_color(theme.accent),
                                        UiTextPositionCache::default(),
                                        Node::default(),
                                    ));
                                });
                        });
                });
        });

    info!("Spawned create context dialog");
}

/// Handle keyboard input in the dialog
fn handle_dialog_input(
    mut commands: Commands,
    mut keyboard: MessageReader<KeyboardInput>,
    mut modal_state: ResMut<ModalDialogOpen>,
    mut input_query: Query<&mut ContextNameInput>,
    mut text_query: Query<&mut MsdfUiText, With<InputTextDisplay>>,
    dialog_query: Query<Entity, With<CreateContextDialog>>,
    theme: Res<Theme>,
    conn: Res<ConnectionCommands>,
) {
    let Ok(mut input) = input_query.single_mut() else {
        return;
    };

    let mut text_changed = false;

    for event in keyboard.read() {
        if !event.state.is_pressed() {
            continue;
        }

        match (&event.logical_key, &event.text) {
            // Backspace - remove last character
            (Key::Backspace, _) => {
                input.text.pop();
                text_changed = true;
            }
            // Enter - submit
            (Key::Enter, _) => {
                if !input.text.is_empty() {
                    submit_create_context(&input.text, &conn);
                    // Close dialog and release modal state
                    modal_state.0 = false;
                    if let Ok(dialog_entity) = dialog_query.single() {
                        commands.entity(dialog_entity).despawn();
                    }
                }
            }
            // Escape - handled by separate system
            (Key::Escape, _) => {}
            // Regular text input
            (_, Some(text)) => {
                // Filter to valid context name characters
                for c in text.chars() {
                    if c.is_alphanumeric() || c == '-' || c == '_' {
                        input.text.push(c);
                        text_changed = true;
                    }
                }
            }
            _ => {}
        }
    }

    // Update the displayed text if it changed
    if text_changed {
        if let Ok(mut msdf_text) = text_query.single_mut() {
            if input.text.is_empty() {
                msdf_text.text = "Enter context name...".to_string();
                msdf_text.color = bevy_to_rgba8(theme.fg_dim);
            } else {
                msdf_text.text = input.text.clone();
                msdf_text.color = bevy_to_rgba8(theme.fg);
            }
        }
    }
}

/// Handle button clicks (submit/cancel)
fn handle_dialog_buttons(
    mut commands: Commands,
    mut modal_state: ResMut<ModalDialogOpen>,
    submit_query: Query<&Interaction, (Changed<Interaction>, With<CreateContextSubmit>)>,
    cancel_query: Query<&Interaction, (Changed<Interaction>, With<CreateContextCancel>)>,
    input_query: Query<&ContextNameInput>,
    dialog_query: Query<Entity, With<CreateContextDialog>>,
    conn: Res<ConnectionCommands>,
) {
    let Ok(dialog_entity) = dialog_query.single() else {
        return;
    };

    // Check submit button
    for interaction in submit_query.iter() {
        if *interaction == Interaction::Pressed {
            if let Ok(input) = input_query.single() {
                if !input.text.is_empty() {
                    submit_create_context(&input.text, &conn);
                    modal_state.0 = false;
                    commands.entity(dialog_entity).despawn();
                }
            }
        }
    }

    // Check cancel button
    for interaction in cancel_query.iter() {
        if *interaction == Interaction::Pressed {
            info!("Create context cancelled");
            modal_state.0 = false;
            commands.entity(dialog_entity).despawn();
        }
    }
}

/// Handle Escape key to close the dialog
fn handle_dialog_escape(
    mut commands: Commands,
    mut modal_state: ResMut<ModalDialogOpen>,
    keys: Res<ButtonInput<KeyCode>>,
    dialog_query: Query<Entity, With<CreateContextDialog>>,
) {
    if keys.just_pressed(KeyCode::Escape) {
        if let Ok(dialog_entity) = dialog_query.single() {
            info!("Create context cancelled (Escape)");
            modal_state.0 = false;
            commands.entity(dialog_entity).despawn();
        }
    }
}

/// Submit the create context request
fn submit_create_context(name: &str, conn: &ConnectionCommands) {
    let instance = Uuid::new_v4().to_string();
    info!("Creating context: {} (instance: {})", name, instance);

    conn.send(ConnectionCommand::JoinContext {
        context: name.to_string(),
        instance,
    });
}
