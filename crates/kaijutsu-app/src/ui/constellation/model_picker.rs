//! Model picker dialog for constellation nodes
//!
//! Pressing `m` on a focused constellation node opens a modal dialog
//! showing available LLM providers/models from `get_llm_config()`.
//! Select with j/k + Enter to call `set_default_model()`.
//!
//! Input is dispatched via `ActionFired` messages from the focus-based
//! input system (FocusArea::Dialog context).

use bevy::prelude::*;

use crate::connection::RpcActor;
use crate::input::action::Action;
use crate::input::events::ActionFired;
use crate::input::focus::{FocusArea, FocusStack};
use crate::text::{MsdfUiText, UiTextPositionCache};
use crate::ui::form::{msdf_label, AsyncSlot, ListItem, SelectableList};
use crate::ui::theme::Theme;

// ============================================================================
// MESSAGES
// ============================================================================

/// Message to open the model picker for a specific context.
#[derive(Message, Clone, Debug)]
pub struct OpenModelPicker {
    pub context_name: String,
}

// ============================================================================
// DATA
// ============================================================================

/// Fetched LLM config data.
#[derive(Clone, Debug)]
pub struct FetchedLlmConfig {
    pub current_provider: String,
    #[allow(dead_code)] // read when model picker UI is wired up
    pub current_model: String,
    /// (provider_name, default_model, available)
    pub models: Vec<(String, String, bool)>,
}

// ============================================================================
// COMPONENTS
// ============================================================================

/// Marker for the model picker dialog overlay.
#[derive(Component)]
pub struct ModelPickerDialog {
    #[allow(dead_code)] // read when model picker UI is wired up
    pub context_name: String,
}

/// Marker for the "Loading..." text while fetching config.
#[derive(Component)]
pub struct ModelPickerLoading;

/// Marker for the list container entity within the dialog.
#[derive(Component)]
struct ModelPickerListContainer;

// ============================================================================
// SYSTEMS
// ============================================================================

pub fn setup_model_picker_systems(app: &mut App) {
    app.add_message::<OpenModelPicker>()
        .init_resource::<AsyncSlot<FetchedLlmConfig>>()
        .add_systems(
            Update,
            (
                handle_open_model_picker,
                poll_model_picker_result,
                handle_model_picker_input,
            )
                .chain(),
        );
}

/// Handle `OpenModelPicker` — kick off async fetch, show loading dialog.
fn handle_open_model_picker(
    mut commands: Commands,
    mut events: MessageReader<OpenModelPicker>,
    mut focus: ResMut<FocusArea>,
    mut focus_stack: ResMut<FocusStack>,
    existing: Query<Entity, With<ModelPickerDialog>>,
    theme: Res<Theme>,
    actor: Option<Res<RpcActor>>,
    result_slot: Res<AsyncSlot<FetchedLlmConfig>>,
) {
    if !existing.is_empty() {
        return;
    }

    for event in events.read() {
        let Some(ref actor) = actor else {
            warn!("Cannot open model picker: no active RPC actor");
            continue;
        };

        focus_stack.push(&mut focus, FocusArea::Dialog);
        result_slot.clear();

        // Spawn loading dialog
        commands
            .spawn((
                ModelPickerDialog {
                    context_name: event.context_name.clone(),
                },
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
                overlay
                    .spawn((
                        Node {
                            width: Val::Px(300.0),
                            min_height: Val::Px(120.0),
                            flex_direction: FlexDirection::Column,
                            padding: UiRect::all(Val::Px(20.0)),
                            border_radius: BorderRadius::all(Val::Px(8.0)),
                            row_gap: Val::Px(12.0),
                            ..default()
                        },
                        BackgroundColor(theme.panel_bg),
                        BorderColor::all(theme.border),
                        Outline::new(Val::Px(1.0), Val::ZERO, theme.border),
                    ))
                    .with_children(|dialog| {
                        msdf_label(
                            dialog,
                            &format!("Model: @{}", event.context_name),
                            16.0,
                            theme.fg,
                        );
                        dialog.spawn((
                            ModelPickerLoading,
                            MsdfUiText::new("Loading models...")
                                .with_font_size(12.0)
                                .with_color(theme.fg_dim),
                            UiTextPositionCache::default(),
                            Node {
                                width: Val::Percent(100.0),
                                height: Val::Px(14.0),
                                ..default()
                            },
                        ));
                        // List container (SelectableList will be inserted here)
                        dialog.spawn((
                            ModelPickerListContainer,
                            Node {
                                width: Val::Percent(100.0),
                                flex_direction: FlexDirection::Column,
                                ..default()
                            },
                        ));
                    });
            });

        // Fetch config async
        let handle = actor.handle.clone();
        let slot = result_slot.sender();

        bevy::tasks::IoTaskPool::get()
            .spawn(async move {
                match handle.get_llm_config().await {
                    Ok(config) => {
                        let models: Vec<_> = config
                            .providers
                            .iter()
                            .map(|p| (p.name.clone(), p.default_model.clone(), p.available))
                            .collect();
                        *slot.lock().unwrap() = Some(FetchedLlmConfig {
                            current_provider: config.default_provider,
                            current_model: config.default_model,
                            models,
                        });
                    }
                    Err(e) => {
                        error!("Failed to fetch LLM config: {}", e);
                    }
                }
            })
            .detach();

        break; // Only handle one open request
    }
}

/// Poll the shared result slot. When data arrives, populate the dialog.
fn poll_model_picker_result(
    mut commands: Commands,
    result_slot: Res<AsyncSlot<FetchedLlmConfig>>,
    loading: Query<Entity, With<ModelPickerLoading>>,
    container_query: Query<Entity, With<ModelPickerListContainer>>,
    existing_list: Query<&SelectableList, With<ModelPickerListContainer>>,
) {
    // Nothing to do if already populated
    if !existing_list.is_empty() {
        return;
    }

    let Some(data) = result_slot.take() else {
        return;
    };

    // Remove loading indicator
    for entity in loading.iter() {
        commands.entity(entity).despawn();
    }

    let Ok(container) = container_query.single() else {
        return;
    };

    // Build list items
    let mut items = Vec::new();
    let mut selected = 0;

    for (i, (provider, model, _available)) in data.models.iter().enumerate() {
        if *provider == data.current_provider {
            selected = i;
        }
        items.push(ListItem::new(format!("{}/{}", provider, model)));
    }

    if items.is_empty() {
        return;
    }

    let mut list = SelectableList::new(items, 13.0);
    list.selected = selected;
    list.selected_bg_alpha = 0.15;
    commands.entity(container).insert(list);
}

/// Handle actions in the model picker (j/k/Enter/Escape via ActionFired).
fn handle_model_picker_input(
    mut commands: Commands,
    mut actions: MessageReader<ActionFired>,
    mut focus: ResMut<FocusArea>,
    mut focus_stack: ResMut<FocusStack>,
    dialogs: Query<Entity, With<ModelPickerDialog>>,
    mut list_query: Query<&mut SelectableList, With<ModelPickerListContainer>>,
    actor: Option<Res<RpcActor>>,
) {
    let Ok(dialog_entity) = dialogs.single() else {
        return;
    };

    if list_query.is_empty() {
        // Still loading — only handle Escape/Unfocus
        for ActionFired(action) in actions.read() {
            if matches!(action, Action::Unfocus) {
                close_model_picker(&mut commands, dialog_entity, &mut focus, &mut focus_stack);
            }
        }
        return;
    }

    for ActionFired(action) in actions.read() {
        match action {
            Action::Unfocus => {
                close_model_picker(&mut commands, dialog_entity, &mut focus, &mut focus_stack);
                return;
            }
            Action::Activate => {
                if let Ok(list) = list_query.single() {
                    if let Some(item) = list.selected_item() {
                        // Parse "provider/model" back into parts
                        if let Some((provider, model)) = item.label.split_once('/') {
                            info!("Model selected: {}/{}", provider, model);

                            if let Some(ref actor) = actor {
                                let handle = actor.handle.clone();
                                let provider = provider.to_string();
                                let model = model.to_string();

                                bevy::tasks::IoTaskPool::get()
                                    .spawn(async move {
                                        match handle.set_default_model(&provider, &model).await {
                                            Ok(true) => info!("Model set to {}/{}", provider, model),
                                            Ok(false) => warn!("set_default_model returned false"),
                                            Err(e) => error!("Failed to set model: {}", e),
                                        }
                                    })
                                    .detach();
                            }
                        }
                    }
                }

                close_model_picker(&mut commands, dialog_entity, &mut focus, &mut focus_stack);
                return;
            }
            Action::FocusNextBlock => {
                if let Ok(mut list) = list_query.single_mut() {
                    list.select_next();
                }
            }
            Action::FocusPrevBlock => {
                if let Ok(mut list) = list_query.single_mut() {
                    list.select_prev();
                }
            }
            _ => {}
        }
    }
}

/// Close the model picker and restore focus from the stack.
fn close_model_picker(
    commands: &mut Commands,
    dialog_entity: Entity,
    focus: &mut FocusArea,
    focus_stack: &mut FocusStack,
) {
    focus_stack.pop(focus);
    commands.entity(dialog_entity).despawn();
}
