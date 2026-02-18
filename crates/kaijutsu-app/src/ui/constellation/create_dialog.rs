//! Create/Fork context dialog for the constellation
//!
//! Provides a modal dialog for creating new contexts (clicking "+") or
//! forking existing contexts (pressing `f` on a focused node).
//!
//! Dialog input is dispatched via `ActionFired`/`TextInputReceived` messages
//! from the focus-based input system (FocusArea::Dialog context).

use bevy::prelude::*;
use uuid::Uuid;

use crate::connection::{BootstrapChannel, BootstrapCommand, RpcActor};
use crate::input::action::Action;
use crate::input::events::{ActionFired, TextInputReceived};
use crate::input::focus::FocusArea;
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

/// Saves the FocusArea before a dialog opens so it can be restored on close.
#[derive(Resource, Default)]
pub struct DialogPreviousFocus(pub Option<FocusArea>);

// ============================================================================
// DIALOG MODE
// ============================================================================

/// What kind of dialog to show.
#[derive(Clone, Debug)]
pub enum DialogMode {
    /// Create a brand new context (from "+" node click)
    CreateContext,
    /// Fork from an existing context (from `f` on focused node)
    ForkContext {
        source_context: String,
        source_document_id: String,
    },
}

/// Message requesting the dialog to open in a specific mode.
#[derive(Message, Clone, Debug)]
pub struct OpenContextDialog(pub DialogMode);

// ============================================================================
// COMPONENTS
// ============================================================================

/// Marker for the "+" create context node in the constellation
#[derive(Component, Default, Reflect)]
#[reflect(Component)]
pub struct CreateContextNode;

/// Marker for the create context dialog (modal overlay).
/// Carries the dialog mode for submit routing.
#[derive(Component)]
pub struct CreateContextDialog {
    pub mode: DialogMode,
}

/// Marker for the text input field in the dialog
#[derive(Component, Default)]
pub struct ContextNameInput {
    /// Current input text
    pub text: String,
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
        .init_resource::<DialogPreviousFocus>()
        .register_type::<CreateContextNode>()
        .add_message::<OpenContextDialog>()
        .add_systems(
            Update,
            (
                handle_create_node_click,
                handle_open_dialog_message,
                handle_dialog_input,
                handle_dialog_buttons,
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

    let node_size = theme.constellation_node_size * 0.8; // Slightly smaller

    // Initial position at origin — update_create_node_visual repositions
    // with camera transform on the next frame.
    let initial_x = 0.0;
    let initial_y = 0.0;

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
                left: Val::Px(initial_x),
                top: Val::Px(initial_y),
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
    mut focus: ResMut<FocusArea>,
    mut prev_focus: ResMut<DialogPreviousFocus>,
    create_nodes: Query<&Interaction, (Changed<Interaction>, With<CreateContextNode>)>,
    existing_dialog: Query<Entity, With<CreateContextDialog>>,
) {
    if !existing_dialog.is_empty() {
        return;
    }

    for interaction in create_nodes.iter() {
        if *interaction == Interaction::Pressed {
            info!("Create context node clicked - spawning dialog");
            modal_state.0 = true;
            prev_focus.0 = Some(focus.clone());
            *focus = FocusArea::Dialog;
            spawn_context_dialog(&mut commands, &theme, DialogMode::CreateContext);
        }
    }
}

/// Handle OpenContextDialog messages (from `f` key on constellation node).
fn handle_open_dialog_message(
    mut commands: Commands,
    theme: Res<Theme>,
    mut modal_state: ResMut<ModalDialogOpen>,
    mut focus: ResMut<FocusArea>,
    mut prev_focus: ResMut<DialogPreviousFocus>,
    mut events: MessageReader<OpenContextDialog>,
    existing_dialog: Query<Entity, With<CreateContextDialog>>,
) {
    if !existing_dialog.is_empty() {
        return;
    }

    for OpenContextDialog(mode) in events.read() {
        info!("Opening context dialog: {:?}", mode);
        modal_state.0 = true;
        prev_focus.0 = Some(focus.clone());
        *focus = FocusArea::Dialog;
        spawn_context_dialog(&mut commands, &theme, mode.clone());
    }
}

/// Spawn the context dialog (modal) — supports both create and fork modes.
fn spawn_context_dialog(commands: &mut Commands, theme: &Theme, mode: DialogMode) {
    let (title_text, placeholder, submit_label) = match &mode {
        DialogMode::CreateContext => (
            "Create New Context".to_string(),
            "Enter context name...".to_string(),
            "Create",
        ),
        DialogMode::ForkContext { source_context, .. } => (
            format!("Fork from @{}", source_context),
            "Enter fork name...".to_string(),
            "Fork",
        ),
    };

    // Modal overlay (darkens background)
    commands
        .spawn((
            CreateContextDialog { mode },
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
            BackgroundColor(theme.modal_backdrop),
            ZIndex(crate::constants::ZLayer::MODAL),
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
                        MsdfUiText::new(&title_text)
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
                            input_field.spawn((
                                InputTextDisplay,
                                MsdfUiText::new(&placeholder)
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

                            // Submit button
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
                                        MsdfUiText::new(submit_label)
                                            .with_font_size(12.0)
                                            .with_color(theme.accent),
                                        UiTextPositionCache::default(),
                                        Node::default(),
                                    ));
                                });
                        });
                });
        });

    info!("Spawned context dialog");
}

/// Handle input in the dialog via ActionFired / TextInputReceived.
fn handle_dialog_input(
    mut commands: Commands,
    mut actions: MessageReader<ActionFired>,
    mut text_events: MessageReader<TextInputReceived>,
    mut modal_state: ResMut<ModalDialogOpen>,
    mut focus: ResMut<FocusArea>,
    mut prev_focus: ResMut<DialogPreviousFocus>,
    mut input_query: Query<&mut ContextNameInput>,
    mut text_query: Query<&mut MsdfUiText, With<InputTextDisplay>>,
    dialog_query: Query<(Entity, &CreateContextDialog)>,
    theme: Res<Theme>,
    bootstrap: Res<BootstrapChannel>,
    conn_state: Res<crate::connection::RpcConnectionState>,
    actor: Option<Res<RpcActor>>,
) {
    let Ok(mut input) = input_query.single_mut() else {
        return;
    };

    let mut text_changed = false;

    // Handle text input (alphanumeric + dashes/underscores only)
    for TextInputReceived(text) in text_events.read() {
        for c in text.chars() {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                input.text.push(c);
                text_changed = true;
            }
        }
    }

    // Handle actions
    for ActionFired(action) in actions.read() {
        match action {
            Action::Backspace => {
                input.text.pop();
                text_changed = true;
            }
            Action::Activate => {
                if !input.text.is_empty()
                    && let Ok((dialog_entity, dialog)) = dialog_query.single()
                {
                    submit_dialog(&input.text, &dialog.mode, &bootstrap, &conn_state, actor.as_deref());
                    close_dialog(&mut commands, dialog_entity, &mut modal_state, &mut focus, &mut prev_focus);
                }
            }
            Action::Unfocus => {
                if let Ok((dialog_entity, _)) = dialog_query.single() {
                    info!("Dialog cancelled (Escape)");
                    close_dialog(&mut commands, dialog_entity, &mut modal_state, &mut focus, &mut prev_focus);
                }
            }
            _ => {}
        }
    }

    if text_changed
        && let Ok(mut msdf_text) = text_query.single_mut()
    {
        let placeholder = if dialog_query
            .single()
            .map(|(_, d)| matches!(d.mode, DialogMode::ForkContext { .. }))
            .unwrap_or(false)
        {
            "Enter fork name..."
        } else {
            "Enter context name..."
        };

        if input.text.is_empty() {
            msdf_text.text = placeholder.to_string();
            msdf_text.color = bevy_to_rgba8(theme.fg_dim);
        } else {
            msdf_text.text = input.text.clone();
            msdf_text.color = bevy_to_rgba8(theme.fg);
        }
    }
}

/// Close the dialog and restore focus to what it was before the dialog opened.
fn close_dialog(
    commands: &mut Commands,
    dialog_entity: Entity,
    modal_state: &mut ModalDialogOpen,
    focus: &mut FocusArea,
    prev_focus: &mut DialogPreviousFocus,
) {
    modal_state.0 = false;
    *focus = prev_focus.0.take().unwrap_or(FocusArea::Constellation);
    commands.entity(dialog_entity).despawn();
}

/// Handle button clicks (submit/cancel)
fn handle_dialog_buttons(
    mut commands: Commands,
    mut modal_state: ResMut<ModalDialogOpen>,
    mut focus: ResMut<FocusArea>,
    mut prev_focus: ResMut<DialogPreviousFocus>,
    submit_query: Query<&Interaction, (Changed<Interaction>, With<CreateContextSubmit>)>,
    cancel_query: Query<&Interaction, (Changed<Interaction>, With<CreateContextCancel>)>,
    input_query: Query<&ContextNameInput>,
    dialog_query: Query<(Entity, &CreateContextDialog)>,
    bootstrap: Res<BootstrapChannel>,
    conn_state: Res<crate::connection::RpcConnectionState>,
    actor: Option<Res<RpcActor>>,
) {
    let Ok((dialog_entity, dialog)) = dialog_query.single() else {
        return;
    };

    for interaction in submit_query.iter() {
        if *interaction == Interaction::Pressed
            && let Ok(input) = input_query.single()
            && !input.text.is_empty()
        {
            submit_dialog(&input.text, &dialog.mode, &bootstrap, &conn_state, actor.as_deref());
            close_dialog(&mut commands, dialog_entity, &mut modal_state, &mut focus, &mut prev_focus);
        }
    }

    for interaction in cancel_query.iter() {
        if *interaction == Interaction::Pressed {
            info!("Dialog cancelled");
            close_dialog(&mut commands, dialog_entity, &mut modal_state, &mut focus, &mut prev_focus);
        }
    }
}

/// Submit the dialog — routes to create or fork depending on mode.
fn submit_dialog(
    name: &str,
    mode: &DialogMode,
    bootstrap: &BootstrapChannel,
    conn_state: &crate::connection::RpcConnectionState,
    actor: Option<&RpcActor>,
) {
    let kernel_id = conn_state
        .current_kernel
        .as_ref()
        .map(|k| k.id.clone())
        .unwrap_or_else(|| crate::constants::DEFAULT_KERNEL_ID.to_string());

    match mode {
        DialogMode::CreateContext => {
            let instance = Uuid::new_v4().to_string();
            info!("Creating context: {} (instance: {})", name, instance);

            let _ = bootstrap.tx.send(BootstrapCommand::SpawnActor {
                config: conn_state.ssh_config.clone(),
                kernel_id,
                context_name: Some(name.to_string()),
                instance,
            });
        }
        DialogMode::ForkContext { source_document_id, source_context } => {
            info!("Forking context: {} from @{} (doc: {})", name, source_context, source_document_id);

            let Some(actor) = actor else {
                error!("Cannot fork: no active RPC actor");
                return;
            };

            // Fork via ActorHandle, then spawn actor to join the new context
            let handle = actor.handle.clone();
            let doc_id = source_document_id.clone();
            let fork_name = name.to_string();
            let config = conn_state.ssh_config.clone();
            let kernel_id = kernel_id.clone();
            let bootstrap_tx = bootstrap.tx.clone();

            bevy::tasks::IoTaskPool::get()
                .spawn(async move {
                    // Use version 0 to fork from latest
                    match handle.fork_from_version(&doc_id, 0, &fork_name).await {
                        Ok(ctx) => {
                            info!("Fork created: {} with {} documents", ctx.name, ctx.documents.len());
                            // Now spawn actor to join the newly forked context
                            let instance = Uuid::new_v4().to_string();
                            let _ = bootstrap_tx.send(BootstrapCommand::SpawnActor {
                                config,
                                kernel_id,
                                context_name: Some(fork_name),
                                instance,
                            });
                        }
                        Err(e) => {
                            error!("Fork failed: {}", e);
                        }
                    }
                })
                .detach();
        }
    }
}
