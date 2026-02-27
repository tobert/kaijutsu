//! Model picker dialog for constellation nodes.
//!
//! Pressing `m` on a focused constellation node opens a modal dialog
//! showing available LLM providers/models from `get_llm_config()`.
//! Select with j/k + Enter to call `set_default_model()`.
//!
//! Uses the Form schema system for UI construction and navigation.

use bevy::prelude::*;

use crate::connection::RpcActor;
use crate::input::action::Action;
use crate::input::events::ActionFired;
use crate::input::focus::{FocusArea, FocusStack};
use crate::ui::form::{
    handle_form_action, AsyncSlot, FieldDesc, Form, FormActionResult, FormFieldContainer,
    FormLayout, FormLoadingText, FormPresentation, ListItem, SelectableList,
};
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

// Field ID for the single model list field.
const FIELD_MODELS: u8 = 0;

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

        // Spawn form entity with schema
        commands.spawn((
            ModelPickerDialog {
                context_name: event.context_name.clone(),
            },
            Form {
                title: format!("Model: @{}", event.context_name),
                layout: FormLayout::Column(vec![FieldDesc {
                    field_id: FIELD_MODELS,
                    label: String::new(),
                    min_height: 0.0,
                    max_height: None,
                    loading_text: Some("Loading models...".into()),
                    bordered: false,
                }]),
                buttons: vec![],
                hints: String::new(),
                field_count: 1,
                initial_field: FIELD_MODELS,
            },
            FormPresentation::Modal {
                width: 300.0,
                min_height: 120.0,
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
        ));

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
    loading: Query<Entity, With<FormLoadingText>>,
    container_query: Query<(Entity, &FormFieldContainer), Without<SelectableList>>,
    existing_list: Query<(&FormFieldContainer, &SelectableList)>,
) {
    // Nothing to do if already populated
    let has_list = existing_list
        .iter()
        .any(|(ffc, _)| ffc.0 == FIELD_MODELS);
    if has_list {
        return;
    }

    let Some(data) = result_slot.take() else {
        return;
    };

    // Remove loading indicator
    for entity in loading.iter() {
        commands.entity(entity).despawn();
    }

    let Some((container, _)) = container_query
        .iter()
        .find(|(_, ffc)| ffc.0 == FIELD_MODELS)
    else {
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

/// Handle actions in the model picker.
fn handle_model_picker_input(
    mut commands: Commands,
    mut actions: MessageReader<ActionFired>,
    mut focus: ResMut<FocusArea>,
    mut focus_stack: ResMut<FocusStack>,
    dialogs: Query<(Entity, &Form), With<ModelPickerDialog>>,
    mut active_field_query: Query<
        &mut crate::ui::form::ActiveFormField,
        With<ModelPickerDialog>,
    >,
    mut list_query: Query<(&FormFieldContainer, &mut SelectableList)>,
    mut tree_query: Query<(&FormFieldContainer, &mut crate::ui::form::TreeView)>,
    actor: Option<Res<RpcActor>>,
) {
    let Ok((dialog_entity, form)) = dialogs.single() else {
        return;
    };

    let has_list = list_query.iter().any(|(ffc, _)| ffc.0 == FIELD_MODELS);

    if !has_list {
        // Still loading — only handle Escape
        for ActionFired(action) in actions.read() {
            if matches!(action, Action::Unfocus) {
                close_model_picker(&mut commands, dialog_entity, &mut focus, &mut focus_stack);
            }
        }
        return;
    }

    let Ok(mut active_field) = active_field_query.single_mut() else {
        return;
    };

    for ActionFired(action) in actions.read() {
        match handle_form_action(action, form, &mut active_field, &mut list_query, &mut tree_query)
        {
            FormActionResult::Cancel => {
                close_model_picker(&mut commands, dialog_entity, &mut focus, &mut focus_stack);
                return;
            }
            FormActionResult::Submit => {
                // Find the model list and get the selected item
                if let Some((_, list)) = list_query.iter().find(|(ffc, _)| ffc.0 == FIELD_MODELS)
                    && let Some(item) = list.selected_item()
                    && let Some((provider, model)) = item.label.split_once('/')
                {
                    info!("Model selected: {}/{}", provider, model);

                    if let Some(ref actor) = actor {
                        let handle = actor.handle.clone();
                        let provider = provider.to_string();
                        let model = model.to_string();

                        bevy::tasks::IoTaskPool::get()
                            .spawn(async move {
                                match handle
                                    .set_default_model(&provider, &model)
                                    .await
                                {
                                    Ok(true) => {
                                        info!("Model set to {}/{}", provider, model)
                                    }
                                    Ok(false) => {
                                        warn!("set_default_model returned false")
                                    }
                                    Err(e) => error!("Failed to set model: {}", e),
                                }
                            })
                            .detach();
                    }
                }

                close_model_picker(&mut commands, dialog_entity, &mut focus, &mut focus_stack);
                return;
            }
            FormActionResult::Consumed | FormActionResult::Ignored => {}
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
